//! Where the processes hort leaves running behind a finished command send their
//! output.
//!
//! A process keeps the streams it was started with. An anchor or a pasta holding
//! the stdout and stderr hort was invoked with therefore keeps them open for as
//! long as the sandbox lives, so a piped or redirected invocation never reaches
//! EOF and its reader waits forever.
//!
//! The output goes to a file rather than nowhere because pasta reports the whole
//! topology it configured, and that report is the only evidence hort would ever
//! have about it. The file lives in the sandbox's own runtime directory, next to
//! what the other surviving processes wrote and under a root the restart empties,
//! so the most a leaked one costs is a file until the machine comes back up.

use std::fs::File;
use std::path::{Path, PathBuf};

const LOG_FILE: &str = "output.log";

/// The log every process hort leaves running for one sandbox writes to.
pub fn sandbox_log_path(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join(LOG_FILE)
}

/// Open a sandbox's log for a process about to be started with it as its output,
/// keeping whatever an earlier process already wrote there.
pub fn open_sandbox_log(sandbox_dir: &Path) -> Result<File, String> {
    let log = sandbox_log_path(sandbox_dir);
    File::options()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|err| format!("opening {}: {err}", log.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::io::Write;

    #[test]
    fn the_sandbox_log_lives_in_the_directory_the_sandbox_dies_with() {
        let sandbox_dir = tempfile::tempdir().unwrap();

        let log = sandbox_log_path(sandbox_dir.path());

        // The sandbox's runtime directory is emptied and taken away when the
        // sandbox is torn down, so a log kept there never outlives the box whose
        // output it holds.
        assert!(log.starts_with(sandbox_dir.path()));
    }

    #[test]
    fn opening_the_sandbox_log_keeps_what_an_earlier_process_wrote() {
        let sandbox_dir = tempfile::tempdir().unwrap();

        let mut anchor_stream = open_sandbox_log(sandbox_dir.path()).unwrap();
        anchor_stream.write_all(b"the anchor spoke\n").unwrap();
        let mut pasta_stream = open_sandbox_log(sandbox_dir.path()).unwrap();
        pasta_stream.write_all(b"pasta spoke\n").unwrap();

        // The processes that survive one sandbox are started at different moments
        // and share one log, so an open that truncated would erase the evidence
        // left by whichever of them spoke first.
        let logged = fs::read_to_string(sandbox_log_path(sandbox_dir.path())).unwrap();
        assert!(logged.contains("the anchor spoke"));
        assert!(logged.contains("pasta spoke"));
    }
}
