//! The dependency-cache directories on the host, under hort's own state.

use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::domain::error::HortError;
use crate::ports::CacheProvider;

const CACHE_DIR: &str = "cache";

/// Creates a project's cache directories on disk, lists what is stored, and
/// collects a project's whole cache. hort owns these, so one that is not there is
/// made rather than reported.
pub struct FileCacheProvider {
    state_root: PathBuf,
}

impl FileCacheProvider {
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn cache_dir(&self) -> PathBuf {
        self.state_root.join(CACHE_DIR)
    }
}

impl CacheProvider for FileCacheProvider {
    fn ensure(&self, sources: &[PathBuf]) -> Result<(), HortError> {
        for source in sources {
            fs::create_dir_all(source).map_err(|error| state_io(source, "create", error))?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>, HortError> {
        let entries = match fs::read_dir(self.cache_dir()) {
            Ok(entries) => entries,
            // Nothing has declared a cache on this machine yet, and a first run
            // that reported that as an error would make an empty machine look
            // like a broken one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(state_io(&self.cache_dir(), "list", error)),
        };

        let mut keys = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            keys.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(keys)
    }

    fn project_exists(&self, project: &Path) -> Result<bool, HortError> {
        // fs::exists and not Path::exists: it answers true or false only for a
        // path it verified either way, and reports the reads it could not make
        // instead of folding them into "not there". The caller gates a deletion
        // on the answer, so the two have to stay apart.
        fs::exists(project).map_err(|error| state_io(project, "read", error))
    }

    fn remove(&self, key: &str) -> Result<(), HortError> {
        let stored = self.cache_dir().join(key);
        match fs::remove_dir_all(&stored) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(state_io(&stored, "remove", error)),
        }
    }
}

fn state_io(path: &Path, action: &str, error: io::Error) -> HortError {
    HortError::StateIo { detail: format!("could not {action} {}: {error}", path.display()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    fn provider(state: &TempDir) -> FileCacheProvider {
        FileCacheProvider::new(state.path().to_path_buf())
    }

    #[test]
    fn file_cache_creates_a_cache_directory_that_is_not_there() {
        let state = TempDir::new().unwrap();
        // The shape of a real address: hort's own state, the one directory name
        // standing for the project, then the entry. Nothing above the entry
        // exists on a project's first run either, so a creation that expects a
        // parent leaves the box binding a source that is not there.
        let source = state
            .path()
            .join("cache")
            .join("%2Fhome%2Ftester%2Fprojects%2Fhort")
            .join("node_modules");

        provider(&state).ensure(std::slice::from_ref(&source)).unwrap();

        // Mounting a bind whose source is missing takes the whole container
        // down, and this source is one hort chose the address of and nothing
        // else on the machine ever creates.
        assert!(source.is_dir());
    }

    #[test]
    fn file_cache_lists_the_project_keys_it_stores() {
        let state = TempDir::new().unwrap();
        let cache = state.path().join("cache");
        fs::create_dir_all(cache.join("%2Fhome%2Ftester%2Fprojects%2Fgone")).unwrap();
        fs::create_dir_all(cache.join("%2Fhome%2Ftester%2Fprojects%2Fhort")).unwrap();

        let mut keys = provider(&state).list().unwrap();
        // The order the disk hands directories back in is not something hort
        // promises, so the assertion holds it to the set and not to a sequence.
        keys.sort();

        assert_eq!(
            keys,
            vec![
                "%2Fhome%2Ftester%2Fprojects%2Fgone".to_string(),
                "%2Fhome%2Ftester%2Fprojects%2Fhort".to_string(),
            ]
        );
    }

    #[test]
    fn file_cache_lists_nothing_without_a_cache_directory() {
        let state = TempDir::new().unwrap();

        let keys = provider(&state).list().unwrap();

        // A user who has never declared a cache still runs prune, and a first
        // run that reports an error for a directory nobody was supposed to
        // create yet turns an empty machine into a broken one.
        assert!(keys.is_empty());
    }

    #[test]
    fn file_cache_removes_a_project_cache_directory() {
        let state = TempDir::new().unwrap();
        let stored = state.path().join("cache").join("%2Fhome%2Ftester%2Fprojects%2Fgone");
        fs::create_dir_all(stored.join("node_modules")).unwrap();

        provider(&state).remove("%2Fhome%2Ftester%2Fprojects%2Fgone").unwrap();

        // What is collected is the whole per-project directory: naming the
        // entries inside it would need the configuration of a project that is
        // gone, which is the one thing nobody can read any more.
        assert!(!stored.exists());
    }

    #[test]
    fn file_cache_reports_a_project_that_is_gone() {
        let state = TempDir::new().unwrap();
        let project = state.path().join("projects").join("gone");

        let exists = provider(&state).project_exists(&project).unwrap();

        assert!(!exists);
    }

    #[test]
    fn file_cache_reports_a_project_that_is_still_there() {
        let state = TempDir::new().unwrap();
        let project = state.path().join("projects").join("hort");
        fs::create_dir_all(&project).unwrap();

        let exists = provider(&state).project_exists(&project).unwrap();

        // This is the answer that protects. A read that reports every project
        // gone reads exactly like an empty machine, and prune would collect the
        // cache of every project the user still works on.
        assert!(exists);
    }
}
