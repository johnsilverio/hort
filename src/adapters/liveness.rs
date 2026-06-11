//! `ProcLivenessProbe` (`LivenessProbe`): stat `/proc/<pid>/ns/mnt` and compare
//! its inode against the token, which guards against PID reuse. Testable without
//! root against the test process's own PID and mount-namespace inode.

use std::fs;
use std::os::unix::fs::MetadataExt;

use crate::domain::model::LivenessToken;
use crate::ports::LivenessProbe;

/// A `LivenessProbe` that reads the kernel process table through `/proc`.
pub struct ProcLivenessProbe;

impl LivenessProbe for ProcLivenessProbe {
    fn is_alive(&self, token: &LivenessToken) -> bool {
        fs::metadata(format!("/proc/{}/ns/mnt", token.pid.0))
            .map(|mnt_ns| mnt_ns.ino() == token.mnt_ns.0)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::MetadataExt;

    use crate::domain::model::{AnchorPid, MountNsInode};

    #[test]
    fn proc_probe_reports_alive_for_own_pid_and_inode() {
        let pid = std::process::id();
        let inode = fs::metadata("/proc/self/ns/mnt").unwrap().ino();
        let token = LivenessToken { pid: AnchorPid(pid), mnt_ns: MountNsInode(inode) };

        assert!(ProcLivenessProbe.is_alive(&token));
    }

    #[test]
    fn proc_probe_reports_dead_for_vanished_pid() {
        let inode = fs::metadata("/proc/self/ns/mnt").unwrap().ino();
        let token = LivenessToken { pid: AnchorPid(u32::MAX), mnt_ns: MountNsInode(inode) };

        assert!(!ProcLivenessProbe.is_alive(&token));
    }

    #[test]
    fn proc_probe_reports_dead_for_mismatched_inode() {
        let pid = std::process::id();
        let inode = fs::metadata("/proc/self/ns/mnt").unwrap().ino();
        let token = LivenessToken { pid: AnchorPid(pid), mnt_ns: MountNsInode(inode + 1) };

        assert!(!ProcLivenessProbe.is_alive(&token));
    }
}
