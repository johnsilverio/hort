//! Teardown as a pure ordered plan: `teardown_plan` returns the mandatory
//! shutdown sequence as data, computed but never executed. The order is
//! filesystem physics, not preference. A worktree deleted while a process still
//! holds it mounted corrupts I/O, so host-side helpers stop before the
//! container, the container before its worktree, and the metadata is removed
//! last.

use crate::domain::model::SandboxRecord;

/// One step of tearing a sandbox down. The mandatory order across the whole plan
/// is the load-bearing guarantee: the steps that stop host-side processes come
/// before the container, the container before its worktree, and the metadata
/// last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownStep {
    /// Stop the host-side notify watcher. Present only when one was spawned.
    StopWatcher,
    /// Stop the host-side network helpers: pasta always, plus the egress proxy
    /// when allowlist mode spawned one.
    StopNetwork,
    /// Tear down the container: its sessions, the anchor, and the namespaces,
    /// releasing the worktree mount.
    StopContainer,
    /// Remove the worktree from the host. Present only in git mode; in no-git
    /// mode the worktree is the user's own folder and is never removed.
    RemoveWorktree,
    /// Remove the on-disk metadata record. Always last.
    RemoveMetadata,
}

/// Build the ordered teardown plan for a sandbox: the mandatory shutdown
/// sequence as data, never executed here. Host-side helpers stop first, then the
/// container, then the worktree (git mode only), then the metadata.
pub fn teardown_plan(record: &SandboxRecord) -> Vec<TeardownStep> {
    let mut plan = Vec::new();
    if record.watcher_pid().is_some() {
        plan.push(TeardownStep::StopWatcher);
    }
    plan.push(TeardownStep::StopNetwork);
    plan.push(TeardownStep::StopContainer);
    if record.branch().is_some() {
        plan.push(TeardownStep::RemoveWorktree);
    }
    plan.push(TeardownStep::RemoveMetadata);
    plan
}

/// Build the ordered undo for a build that failed once the anchor was already
/// standing: the same mandatory sequence, up to and including the container, and
/// nothing after it. The host-side helpers stop first, then the container; the
/// worktree and the metadata record are left where they are.
pub fn rollback_plan(record: &SandboxRecord) -> Vec<TeardownStep> {
    let mut plan = teardown_plan(record);
    // A plan with no container step truncates to nothing rather than to all of
    // itself: everything past the container is what the undo must never do.
    let kept = plan.iter().position(|step| *step == TeardownStep::StopContainer);
    plan.truncate(kept.map_or(0, |index| index + 1));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BranchName, SandboxName};
    use std::path::PathBuf;

    fn git_record() -> SandboxRecord {
        record_with_branch(Some(BranchName::new("demo").unwrap()))
    }

    fn no_git_record() -> SandboxRecord {
        record_with_branch(None)
    }

    fn record_with_branch(branch: Option<BranchName>) -> SandboxRecord {
        SandboxRecord::new(
            SandboxName::new("demo").unwrap(),
            branch,
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-10T12:00:00Z".to_string(),
            "2026-06-10T12:00:00Z".to_string(),
            None,
            PathBuf::from("/home/tester/projects/demo"),
        )
    }

    fn position(plan: &[TeardownStep], step: TeardownStep) -> usize {
        plan.iter().position(|s| *s == step).expect("step present in plan")
    }

    #[test]
    fn teardown_plan_orders_helpers_before_container() {
        let plan = teardown_plan(&git_record().with_watcher_pid(99));

        let container = position(&plan, TeardownStep::StopContainer);
        assert!(position(&plan, TeardownStep::StopWatcher) < container);
        assert!(position(&plan, TeardownStep::StopNetwork) < container);
    }

    #[test]
    fn teardown_plan_orders_container_before_worktree() {
        let plan = teardown_plan(&git_record());

        assert!(
            position(&plan, TeardownStep::StopContainer)
                < position(&plan, TeardownStep::RemoveWorktree)
        );
    }

    #[test]
    fn teardown_plan_removes_metadata_last() {
        let plan = teardown_plan(&git_record());

        assert_eq!(plan.last(), Some(&TeardownStep::RemoveMetadata));
    }

    #[test]
    fn teardown_plan_omits_worktree_in_no_git() {
        let plan = teardown_plan(&no_git_record());

        assert!(!plan.contains(&TeardownStep::RemoveWorktree));
        assert!(plan.contains(&TeardownStep::StopContainer));
        assert!(plan.contains(&TeardownStep::RemoveMetadata));
    }

    #[test]
    fn teardown_plan_skips_watcher_when_absent() {
        let plan = teardown_plan(&git_record());

        assert!(!plan.contains(&TeardownStep::StopWatcher));
        assert!(plan.contains(&TeardownStep::StopNetwork));
    }

    #[test]
    fn rollback_plan_stops_at_the_container() {
        let plan = rollback_plan(&git_record());

        // The two steps it leaves out are exactly the two that touch the disk.
        // A failed build undoes what it started, never what holds work: the
        // worktree may carry changes from an earlier run, and the record is what
        // the next run reads to finish or clean this sandbox.
        assert_eq!(plan, vec![TeardownStep::StopNetwork, TeardownStep::StopContainer]);
    }
}
