//! Per-record liveness reconciliation: `SandboxRecord::reconcile` asks a
//! `LivenessProbe` whether the recorded anchor is still the live one, yielding
//! `Live` or `Orphaned`. The two cross-source verdicts are not decidable from a
//! single record and belong to the reconciler that compares records against the
//! live-anchor registry and the worktree list.

use crate::domain::model::{SandboxName, SandboxRecord};
use crate::ports::{LivenessProbe, RegistryEntry, Worktree};

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

/// Reconcile the on-disk records against the live anchor registry and the
/// worktree list, the cross-source verdict a single record cannot reach on its
/// own. Observable reality wins over the recorded memory of intent.
///
/// For each record the result carries `Live` when a live registry entry holds
/// its token and its worktree path is present, `Inconsistent` when the anchor
/// is live but the worktree path is gone, and `Orphaned` when no live entry
/// holds its token. Each live entry whose id names no record yields a
/// `LostRecord` pair. The order of the returned pairs is unspecified.
pub fn reconcile_all(
    records: &[SandboxRecord],
    live_anchors: &[RegistryEntry],
    worktrees: &[Worktree],
) -> Vec<(SandboxName, SandboxState)> {
    let mut verdicts = Vec::new();

    for record in records {
        let anchor_live = record
            .liveness_token()
            .is_some_and(|token| live_anchors.iter().any(|entry| entry.token == token));
        let worktree_present = worktrees
            .iter()
            .any(|worktree| worktree.path == *record.worktree_path());

        let state = match (anchor_live, worktree_present) {
            (true, true) => SandboxState::Live,
            (true, false) => SandboxState::Inconsistent,
            (false, _) => SandboxState::Orphaned,
        };
        verdicts.push((record.name().clone(), state));
    }

    for entry in live_anchors {
        let has_record = records.iter().any(|record| record.name() == &entry.id);
        if !has_record {
            verdicts.push((entry.id.clone(), SandboxState::LostRecord));
        }
    }

    verdicts
}

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

    fn record_at(name: &str, worktree_path: &str) -> SandboxRecord {
        SandboxRecord::new(
            SandboxName::new(name).unwrap(),
            BranchName::new(name).unwrap(),
            PathBuf::from(worktree_path),
            PathBuf::from("/state/sandboxes/overlay"),
            "2026-06-10T12:00:00Z".to_string(),
            "2026-06-10T12:00:00Z".to_string(),
            None,
        )
    }

    fn entry(name: &str, token: LivenessToken) -> RegistryEntry {
        RegistryEntry { id: SandboxName::new(name).unwrap(), token }
    }

    fn worktree(path: &str) -> Worktree {
        Worktree { path: PathBuf::from(path) }
    }

    #[test]
    fn reconciler_reports_live_for_intact_sandbox() {
        let anchor = token(1234, 42);
        let records =
            vec![record_at("demo", "/state/sandboxes/demo/worktree-demo").with_token(anchor)];
        let live = vec![entry("demo", anchor)];
        let worktrees = vec![worktree("/state/sandboxes/demo/worktree-demo")];

        let result = reconcile_all(&records, &live, &worktrees);

        assert!(result.contains(&(SandboxName::new("demo").unwrap(), SandboxState::Live)));
    }

    #[test]
    fn reconciler_prefers_observable_reality() {
        let records = vec![
            record_at("demo", "/state/sandboxes/demo/worktree-demo").with_token(token(1234, 42)),
        ];
        let live: Vec<RegistryEntry> = Vec::new();
        let worktrees = vec![worktree("/state/sandboxes/demo/worktree-demo")];

        let result = reconcile_all(&records, &live, &worktrees);

        assert!(result.contains(&(SandboxName::new("demo").unwrap(), SandboxState::Orphaned)));
    }

    #[test]
    fn reconciler_flags_lost_record_for_live_anchor_without_metadata() {
        let records: Vec<SandboxRecord> = Vec::new();
        let live = vec![entry("ghost", token(4321, 7))];
        let worktrees: Vec<Worktree> = Vec::new();

        let result = reconcile_all(&records, &live, &worktrees);

        assert!(result.contains(&(SandboxName::new("ghost").unwrap(), SandboxState::LostRecord)));
    }

    #[test]
    fn reconciler_flags_inconsistent_when_worktree_gone() {
        let anchor = token(1234, 42);
        let records =
            vec![record_at("demo", "/state/sandboxes/demo/worktree-demo").with_token(anchor)];
        let live = vec![entry("demo", anchor)];
        let worktrees: Vec<Worktree> = Vec::new();

        let result = reconcile_all(&records, &live, &worktrees);

        assert!(result.contains(&(SandboxName::new("demo").unwrap(), SandboxState::Inconsistent)));
    }
}
