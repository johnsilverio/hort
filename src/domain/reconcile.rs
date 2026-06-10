//! Per-record liveness reconciliation: `SandboxRecord::reconcile` asks a
//! `LivenessProbe` whether the recorded anchor is still the live one, yielding
//! `Live` or `Orphaned`. The two cross-source verdicts are not decidable from a
//! single record and belong to the reconciler that compares records against the
//! live-anchor registry and the worktree list.

use crate::domain::model::SandboxRecord;
use crate::ports::LivenessProbe;

/// The reconciled state of a sandbox. A single record can only distinguish
/// `Live` from `Orphaned`. `LostRecord` (a live anchor with no record) and
/// `Inconsistent` (a live anchor whose worktree vanished) require cross-checking
/// every source, so they are produced by the cross-source reconciler, not by
/// [`SandboxRecord::reconcile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    Live,
    Orphaned,
    LostRecord,
    Inconsistent,
}

impl SandboxRecord {
    /// Reconcile this record against the kernel: `Live` iff the record carries a
    /// liveness token and the probe confirms that exact token alive, otherwise
    /// `Orphaned`.
    pub fn reconcile(&self, probe: &dyn LivenessProbe) -> SandboxState {
        match self.liveness_token() {
            Some(token) if probe.is_alive(&token) => SandboxState::Live,
            _ => SandboxState::Orphaned,
        }
    }
}

// TODO(D-07): the cross-source reconciler adding `LostRecord` and `Inconsistent`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{AnchorPid, BranchName, LivenessToken, MountNsInode, SandboxName};
    use std::path::PathBuf;

    fn fresh_record() -> SandboxRecord {
        SandboxRecord::new(
            SandboxName::new("demo").unwrap(),
            BranchName::new("demo").unwrap(),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-10T12:00:00Z".to_string(),
            "2026-06-10T12:00:00Z".to_string(),
            None,
        )
    }

    fn token(pid: u32, inode: u64) -> LivenessToken {
        LivenessToken { pid: AnchorPid(pid), mnt_ns: MountNsInode(inode) }
    }

    /// A probe scripted over full tokens: it considers exactly the tokens it was
    /// handed alive, so a recycled PID under a different inode never counts.
    struct ScriptedProbe {
        alive: Vec<LivenessToken>,
    }

    impl LivenessProbe for ScriptedProbe {
        fn is_alive(&self, token: &LivenessToken) -> bool {
            self.alive.contains(token)
        }
    }

    #[test]
    fn record_is_live_when_token_alive() {
        let recorded = token(1234, 42);
        let record = fresh_record().with_token(recorded);
        let probe = ScriptedProbe { alive: vec![recorded] };

        assert_eq!(record.reconcile(&probe), SandboxState::Live);
    }

    #[test]
    fn record_is_orphaned_when_pid_gone() {
        let recorded = token(1234, 42);
        let record = fresh_record().with_token(recorded);
        let probe = ScriptedProbe { alive: vec![] };

        assert_eq!(record.reconcile(&probe), SandboxState::Orphaned);
    }

    #[test]
    fn record_is_orphaned_when_inode_mismatches() {
        let record = fresh_record().with_token(token(1234, 42));
        let probe = ScriptedProbe { alive: vec![token(1234, 99)] };

        assert_eq!(record.reconcile(&probe), SandboxState::Orphaned);
    }

    #[test]
    fn record_reconciles_orphaned_before_anchor_starts() {
        let record = fresh_record();
        let probe = ScriptedProbe { alive: vec![] };

        assert_eq!(record.reconcile(&probe), SandboxState::Orphaned);
    }
}
