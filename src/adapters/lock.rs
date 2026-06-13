//! `FlockSandboxLock`: the real `SandboxLock`, an advisory file lock that
//! serializes the build of one sandbox name.
//!
//! The lock is a `flock`-backed advisory lock on a file inside the sandbox's
//! state directory. The kernel holds it per open file handle and releases it
//! when every handle to that file closes, so a build that dies without
//! releasing still frees the name and a crashed build never wedges it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::PathBuf;

use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::ports::SandboxLock;

const SANDBOXES_DIR: &str = "sandboxes";
const LOCK_FILE: &str = "build.lock";

/// A `SandboxLock` backed by an advisory file lock under `state_root` (the
/// `~/.local/state/hort` directory).
pub struct FlockSandboxLock {
    state_root: PathBuf,
    held: RefCell<HashMap<String, File>>,
}

impl FlockSandboxLock {
    /// Build a lock rooted at `state_root`.
    pub fn new(state_root: PathBuf) -> Self {
        Self {
            state_root,
            held: RefCell::new(HashMap::new()),
        }
    }

    fn lock_path(&self, name: &SandboxName) -> PathBuf {
        self.state_root.join(SANDBOXES_DIR).join(name.as_str()).join(LOCK_FILE)
    }
}

impl SandboxLock for FlockSandboxLock {
    fn try_acquire(&self, name: &SandboxName) -> Result<bool, HortError> {
        let lock_path = self.lock_path(name);
        let sandbox_dir = lock_path.parent().expect("a lock path always has a parent directory");
        fs::create_dir_all(sandbox_dir).map_err(|error| HortError::StateIo {
            detail: format!("could not create {}: {error}", sandbox_dir.display()),
        })?;

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| HortError::StateIo {
                detail: format!("could not open {}: {error}", lock_path.display()),
            })?;

        match file.try_lock() {
            Ok(()) => {
                // The flock lives only as long as the open handle, so keep the
                // file in the instance; dropping it here would release the lock
                // at once and let a second build clear the same-name guard.
                self.held.borrow_mut().insert(name.as_str().to_owned(), file);
                Ok(true)
            }
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(HortError::StateIo {
                detail: format!("could not lock {}: {error}", lock_path.display()),
            }),
        }
    }

    fn release(&self, name: &SandboxName) -> Result<(), HortError> {
        // Drop the handle to release the flock, but never unlink the lockfile:
        // another open handle on the same path would keep the name acquirable
        // and let two builds hold it at once. Removing the file is the store's
        // job, as part of removing the whole sandbox directory.
        self.held.borrow_mut().remove(name.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flock_lock_acquires_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let lock = FlockSandboxLock::new(dir.path().to_path_buf());

        let acquired = lock.try_acquire(&SandboxName::new("demo").unwrap()).unwrap();

        assert!(acquired);
    }

    #[test]
    fn flock_lock_refuses_name_held_by_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let name = SandboxName::new("demo").unwrap();
        let holder = FlockSandboxLock::new(dir.path().to_path_buf());
        holder.try_acquire(&name).unwrap();
        let contender = FlockSandboxLock::new(dir.path().to_path_buf());

        let acquired = contender.try_acquire(&name).unwrap();

        assert!(!acquired);
    }

    #[test]
    fn flock_lock_acquires_name_again_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let name = SandboxName::new("demo").unwrap();
        let holder = FlockSandboxLock::new(dir.path().to_path_buf());
        holder.try_acquire(&name).unwrap();
        holder.release(&name).unwrap();
        let contender = FlockSandboxLock::new(dir.path().to_path_buf());

        let reacquired = contender.try_acquire(&name).unwrap();

        assert!(reacquired);
    }

    #[test]
    fn flock_lock_scopes_locks_per_name() {
        let dir = tempfile::tempdir().unwrap();
        let holder = FlockSandboxLock::new(dir.path().to_path_buf());
        holder.try_acquire(&SandboxName::new("demo").unwrap()).unwrap();
        let contender = FlockSandboxLock::new(dir.path().to_path_buf());

        let acquired_other =
            contender.try_acquire(&SandboxName::new("other").unwrap()).unwrap();

        assert!(acquired_other);
    }

    #[test]
    fn flock_lock_releases_lock_when_holder_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let name = SandboxName::new("demo").unwrap();
        let holder = FlockSandboxLock::new(dir.path().to_path_buf());
        holder.try_acquire(&name).unwrap();
        // Drop without release: a build that dies frees the name on its own, so
        // the next build must be able to acquire it.
        std::mem::drop(holder);
        let contender = FlockSandboxLock::new(dir.path().to_path_buf());

        let reacquired = contender.try_acquire(&name).unwrap();

        assert!(reacquired);
    }
}
