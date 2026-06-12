//! `FileMetadataStore`: the on-disk home of each sandbox's record, one JSON file
//! per sandbox at `<state_root>/sandboxes/<name>/metadata.json`.
//!
//! Writes are atomic: the record lands in a temp file in the same directory and
//! is renamed over the final path, so a crash mid-write can never leave a
//! half-written record that a later read would mistake for corruption. Reads
//! validate what they load (schema version, names, timestamps); a file that fails
//! validation reads as corrupt, and `list` skips both corrupt and half-built
//! entries so one rotten file never hides every healthy sandbox.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::error::HortError;
use crate::domain::idle::parse_timestamp;
use crate::domain::model::{SandboxName, SandboxRecord};
use crate::ports::{CorruptEntry, MetadataStore};

const SANDBOXES_DIR: &str = "sandboxes";
const METADATA_FILE: &str = "metadata.json";
const TEMP_FILE: &str = "metadata.json.tmp";
const SCHEMA_VERSION: u64 = 1;

/// A `MetadataStore` backed by one JSON file per sandbox under `state_root` (the
/// `~/.local/state/hort` directory).
pub struct FileMetadataStore {
    state_root: PathBuf,
}

impl FileMetadataStore {
    /// Build a store rooted at `state_root`.
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.state_root.join(SANDBOXES_DIR)
    }

    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.sandboxes_dir().join(name.as_str())
    }
}

impl MetadataStore for FileMetadataStore {
    fn put(&self, record: &SandboxRecord) -> Result<(), HortError> {
        let sandbox_dir = self.sandbox_dir(record.name());
        fs::create_dir_all(&sandbox_dir)
            .map_err(|error| corrupt(format!("could not create {}: {error}", sandbox_dir.display())))?;

        let serialized = serde_json::to_vec_pretty(record).map_err(|error| {
            corrupt(format!("could not serialize record for '{}': {error}", record.name().as_str()))
        })?;

        let temp_path = sandbox_dir.join(TEMP_FILE);
        fs::write(&temp_path, &serialized)
            .map_err(|error| corrupt(format!("could not write {}: {error}", temp_path.display())))?;
        fs::rename(&temp_path, sandbox_dir.join(METADATA_FILE)).map_err(|error| {
            corrupt(format!("could not persist metadata for '{}': {error}", record.name().as_str()))
        })
    }

    fn get(&self, name: &SandboxName) -> Result<Option<SandboxRecord>, HortError> {
        load(&self.sandbox_dir(name).join(METADATA_FILE))
    }

    fn list(&self) -> Result<Vec<SandboxRecord>, HortError> {
        let entries = match fs::read_dir(self.sandboxes_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(corrupt(format!(
                    "could not list sandboxes at {}: {error}",
                    self.sandboxes_dir().display()
                )));
            }
        };

        let mut records = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if let Ok(Some(record)) = load(&entry.path().join(METADATA_FILE)) {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        match fs::remove_dir_all(self.sandbox_dir(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(corrupt(format!(
                "could not remove sandbox state for '{}': {error}",
                name.as_str()
            ))),
        }
    }

    fn list_corrupt(&self) -> Result<Vec<CorruptEntry>, HortError> {
        let entries = match fs::read_dir(self.sandboxes_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(corrupt(format!(
                    "could not list sandboxes at {}: {error}",
                    self.sandboxes_dir().display()
                )));
            }
        };

        let mut corrupt_entries = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            // A loadable record (Some) or a half-built dir with no metadata file
            // (None) is healthy; only a load error is corruption to surface.
            if let Err(error) = load(&entry.path().join(METADATA_FILE)) {
                corrupt_entries.push(CorruptEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    detail: detail_of(error),
                });
            }
        }
        Ok(corrupt_entries)
    }
}

/// The human-readable reason a record failed to load. A load failure is always a
/// `CorruptMetadata`, but any other variant's `Display` is an acceptable fallback.
fn detail_of(error: HortError) -> String {
    match error {
        HortError::CorruptMetadata { detail } => detail,
        other => other.to_string(),
    }
}

/// Read and validate one record file, the single load path `get` and `list`
/// share. A missing file is `Ok(None)` (the sandbox dir was never built, or this
/// subdir is half-built); a present file that fails any check (unreadable JSON,
/// an invalid name or timestamp, an unknown schema version) is corrupt.
fn load(path: &Path) -> Result<Option<SandboxRecord>, HortError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(corrupt(format!("could not read metadata at {}: {error}", path.display())));
        }
    };

    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| corrupt(format!("could not parse metadata at {}: {error}", path.display())))?;

    match value.get("schemaVersion").and_then(serde_json::Value::as_u64) {
        Some(SCHEMA_VERSION) => {}
        Some(version) => {
            return Err(corrupt(format!(
                "unsupported metadata schema version {version} at {} (expected {SCHEMA_VERSION})",
                path.display()
            )));
        }
        None => {
            return Err(corrupt(format!("missing schemaVersion in metadata at {}", path.display())));
        }
    }

    let record: SandboxRecord = serde_json::from_value(value)
        .map_err(|error| corrupt(format!("invalid metadata record at {}: {error}", path.display())))?;

    parse_timestamp(record.created_at())
        .map_err(|error| corrupt(format!("invalid createdAt at {}: {error}", path.display())))?;
    parse_timestamp(record.last_attach_at())
        .map_err(|error| corrupt(format!("invalid lastAttachAt at {}: {error}", path.display())))?;

    Ok(Some(record))
}

fn corrupt(detail: String) -> HortError {
    HortError::CorruptMetadata { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    use crate::fakes::{
        metadata_store_lists_all_put_records, metadata_store_put_overwrites_existing_record,
        metadata_store_remove_is_idempotent_for_missing_name,
        metadata_store_remove_makes_record_missing, metadata_store_returns_none_for_missing_name,
        metadata_store_round_trips_record, sample_record,
    };

    fn write_metadata(state_root: &Path, name: &str, contents: &str) {
        let sandbox_dir = state_root.join("sandboxes").join(name);
        fs::create_dir_all(&sandbox_dir).unwrap();
        fs::write(sandbox_dir.join("metadata.json"), contents).unwrap();
    }

    #[test]
    fn file_store_round_trips_record() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_round_trips_record(FileMetadataStore::new(dir.path().to_path_buf()));
    }

    #[test]
    fn file_store_returns_none_for_missing_name() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_returns_none_for_missing_name(FileMetadataStore::new(dir.path().to_path_buf()));
    }

    #[test]
    fn file_store_put_overwrites_existing_record() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_put_overwrites_existing_record(FileMetadataStore::new(dir.path().to_path_buf()));
    }

    #[test]
    fn file_store_lists_all_put_records() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_lists_all_put_records(FileMetadataStore::new(dir.path().to_path_buf()));
    }

    #[test]
    fn file_store_remove_makes_record_missing() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_remove_makes_record_missing(FileMetadataStore::new(dir.path().to_path_buf()));
    }

    #[test]
    fn file_store_remove_is_idempotent_for_missing_name() {
        let dir = tempfile::tempdir().unwrap();
        metadata_store_remove_is_idempotent_for_missing_name(FileMetadataStore::new(
            dir.path().to_path_buf(),
        ));
    }

    #[test]
    fn file_store_get_errs_on_unreadable_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        write_metadata(dir.path(), "demo", "this is not valid json");

        let result = store.get(&SandboxName::new("demo").unwrap());

        assert!(matches!(result, Err(HortError::CorruptMetadata { .. })));
    }

    #[test]
    fn file_store_get_errs_on_invalid_name_in_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        write_metadata(
            dir.path(),
            "demo",
            r#"{
                "schemaVersion": 1,
                "name": "bad/name",
                "branch": "demo",
                "worktreePath": "/state/sandboxes/demo/worktree-demo",
                "overlayPath": "/state/sandboxes/demo/overlay",
                "createdAt": "2026-06-11T12:00:00Z",
                "lastAttachAt": "2026-06-11T12:00:00Z",
                "notifyChannel": null,
                "watcherPid": null,
                "token": null
            }"#,
        );

        let result = store.get(&SandboxName::new("demo").unwrap());

        assert!(matches!(result, Err(HortError::CorruptMetadata { .. })));
    }

    #[test]
    fn file_store_get_errs_on_invalid_timestamp_in_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        write_metadata(
            dir.path(),
            "demo",
            r#"{
                "schemaVersion": 1,
                "name": "demo",
                "branch": "demo",
                "worktreePath": "/state/sandboxes/demo/worktree-demo",
                "overlayPath": "/state/sandboxes/demo/overlay",
                "createdAt": "not a timestamp",
                "lastAttachAt": "2026-06-11T12:00:00Z",
                "notifyChannel": null,
                "watcherPid": null,
                "token": null
            }"#,
        );

        let result = store.get(&SandboxName::new("demo").unwrap());

        assert!(matches!(result, Err(HortError::CorruptMetadata { .. })));
    }

    #[test]
    fn file_store_get_errs_on_unknown_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        write_metadata(
            dir.path(),
            "demo",
            r#"{
                "schemaVersion": 999,
                "name": "demo",
                "branch": "demo",
                "worktreePath": "/state/sandboxes/demo/worktree-demo",
                "overlayPath": "/state/sandboxes/demo/overlay",
                "createdAt": "2026-06-11T12:00:00Z",
                "lastAttachAt": "2026-06-11T12:00:00Z",
                "notifyChannel": null,
                "watcherPid": null,
                "token": null
            }"#,
        );

        let result = store.get(&SandboxName::new("demo").unwrap());

        assert!(matches!(result, Err(HortError::CorruptMetadata { .. })));
    }

    #[test]
    fn file_store_list_skips_corrupt_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        store.put(&sample_record("good")).unwrap();
        write_metadata(dir.path(), "broken", "this is not valid json");

        let listed = store.list().unwrap();

        assert_eq!(listed, vec![sample_record("good")]);
    }

    #[test]
    fn file_store_lists_nothing_for_missing_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());

        let listed = store.list().unwrap();

        assert!(listed.is_empty());
    }

    #[test]
    fn file_store_remove_clears_sandbox_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        store.put(&sample_record("demo")).unwrap();
        let sandbox_dir = dir.path().join("sandboxes").join("demo");
        fs::write(sandbox_dir.join("notify-debris"), "x").unwrap();

        store.remove(&SandboxName::new("demo").unwrap()).unwrap();

        assert!(!sandbox_dir.exists());
    }

    #[test]
    fn file_store_lists_corrupt_entry_with_detail() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        write_metadata(
            dir.path(),
            "demo",
            r#"{
                "schemaVersion": 1,
                "name": "bad/name",
                "branch": "demo",
                "worktreePath": "/state/sandboxes/demo/worktree-demo",
                "overlayPath": "/state/sandboxes/demo/overlay",
                "createdAt": "2026-06-11T12:00:00Z",
                "lastAttachAt": "2026-06-11T12:00:00Z",
                "notifyChannel": null,
                "watcherPid": null,
                "token": null
            }"#,
        );

        let corrupt = store.list_corrupt().unwrap();

        assert_eq!(corrupt.len(), 1);
        assert_eq!(corrupt[0].name, "demo");
        assert!(!corrupt[0].detail.is_empty());
    }

    #[test]
    fn file_store_list_corrupt_skips_valid_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        store.put(&sample_record("demo")).unwrap();

        let corrupt = store.list_corrupt().unwrap();

        assert!(corrupt.is_empty());
    }

    #[test]
    fn file_store_list_corrupt_skips_half_built_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());
        fs::create_dir_all(dir.path().join("sandboxes").join("demo")).unwrap();

        let corrupt = store.list_corrupt().unwrap();

        assert!(corrupt.is_empty());
    }

    #[test]
    fn file_store_list_corrupt_is_empty_for_missing_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMetadataStore::new(dir.path().to_path_buf());

        let corrupt = store.list_corrupt().unwrap();

        assert!(corrupt.is_empty());
    }
}
