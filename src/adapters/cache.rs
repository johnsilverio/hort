//! The dependency-cache directories on the host, under hort's own state.

use std::fs;
use std::path::PathBuf;

use crate::domain::error::HortError;
use crate::ports::CacheProvider;

/// Creates a project's cache directories on disk. hort owns these, so one that
/// is not there is made rather than reported.
pub struct FileCacheProvider;

impl CacheProvider for FileCacheProvider {
    fn ensure(&self, sources: &[PathBuf]) -> Result<(), HortError> {
        for source in sources {
            fs::create_dir_all(source).map_err(|error| HortError::StateIo {
                detail: format!("could not create {}: {error}", source.display()),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

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

        FileCacheProvider.ensure(std::slice::from_ref(&source)).unwrap();

        // Mounting a bind whose source is missing takes the whole container
        // down, and this source is one hort chose the address of and nothing
        // else on the machine ever creates.
        assert!(source.is_dir());
    }
}
