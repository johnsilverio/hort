//! `FileMetadataStore` (`MetadataStore`): the on-disk `SandboxRecord` JSON under
//! `~/.local/state/hort/sandboxes/<name>/metadata.json` (ADR-0010). Round-trip
//! is identity; a missing record is `Ok(None)`; `schemaVersion` guards changes.
//!
//! See backlog A-01.

// TODO(A-01): the file-backed store + tempdir contract test.
