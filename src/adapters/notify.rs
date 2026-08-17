//! The completion channel on the host: `FileNotifyProvider`, the per-sandbox
//! directory an agent's hook writes its events into from inside the box.
//!
//! The channel lives with what remembers the sandbox rather than with what a
//! restart makes meaningless: a completion that happened stays a fact, and the
//! events file is also how long ago the box last did something.

use std::fs;
use std::path::PathBuf;

use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::ports::NotifyProvider;

// TODO(A-07b): the desktop notifier and the inotify watcher of this directory.

const SANDBOXES_DIR: &str = "sandboxes";
const CHANNEL_DIR: &str = "notify";

/// Makes a sandbox's channel directory on disk, under the state hort keeps for
/// that sandbox. hort owns the directory, so one that is not there is made rather
/// than reported.
pub struct FileNotifyProvider {
    state_root: PathBuf,
}

impl FileNotifyProvider {
    /// Build a provider keeping every channel under `state_root`, beside the
    /// record and the overlay of the sandbox it belongs to.
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }
}

impl NotifyProvider for FileNotifyProvider {
    fn ensure(&self, name: &SandboxName) -> Result<PathBuf, HortError> {
        let channel = self.state_root.join(SANDBOXES_DIR).join(name.as_str()).join(CHANNEL_DIR);
        fs::create_dir_all(&channel).map_err(|error| HortError::StateIo {
            detail: format!("could not create {}: {error}", channel.display()),
        })?;
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn the_channel_is_made_under_the_state_of_the_sandbox_it_belongs_to() {
        let state = TempDir::new().unwrap();
        let provider = FileNotifyProvider::new(state.path().to_path_buf());

        let channel = provider.ensure(&SandboxName::new("demo").unwrap()).unwrap();

        // Both halves of one guarantee, and neither is worth anything alone: the
        // path it answers with is what the container binds, so a path it reports
        // without making takes the whole box down, and a directory it makes
        // somewhere else is a channel the watcher will never find. Nothing above
        // this derives the address a second time, which is why it is the one
        // asserted here.
        assert_eq!(channel, state.path().join("sandboxes").join("demo").join("notify"));
        assert!(channel.is_dir());
    }

    #[test]
    fn a_channel_that_is_already_there_is_not_an_error() {
        let state = TempDir::new().unwrap();
        let provider = FileNotifyProvider::new(state.path().to_path_buf());
        let name = SandboxName::new("demo").unwrap();
        provider.ensure(&name).unwrap();

        // `up` is reentrant against a half-built sandbox, so the second run of a
        // build that broke reaches this again, and a box that already has its
        // channel is a box in the state this was asked for.
        assert!(provider.ensure(&name).is_ok());
    }
}
