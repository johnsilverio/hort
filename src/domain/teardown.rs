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
    /// Stop the host-side notify watcher.
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

/// The steps that stop something still running, in the order the kernel makes
/// mandatory: host-side helpers, then the container that holds the worktree
/// mount. Every plan in this module opens with exactly this sequence and they
/// differ only in what they add after it, so the order is authored once. Written
/// out twice it would drift the first time a step joins the family, because the
/// compiler forces a new enum variant into every dispatch and into no plan.
///
/// Which host-side helpers a sandbox actually left running is the providers'
/// answer and not this one's, the way the network step already covers pasta
/// alone and pasta with a proxy without the plan knowing which.
fn stop_sequence() -> Vec<TeardownStep> {
    vec![TeardownStep::StopWatcher, TeardownStep::StopNetwork, TeardownStep::StopContainer]
}

/// Build the ordered teardown plan for a sandbox: the mandatory shutdown
/// sequence as data, never executed here. Host-side helpers stop first, then the
/// container, then the worktree (git mode only), then the metadata.
pub fn teardown_plan(record: &SandboxRecord) -> Vec<TeardownStep> {
    let mut plan = stop_sequence();
    if record.branch().is_some() {
        plan.push(TeardownStep::RemoveWorktree);
    }
    plan.push(TeardownStep::RemoveMetadata);
    plan
}

/// Build the ordered teardown plan for a sandbox the kernel is running and hort
/// has no record of: the same mandatory sequence, up to and including the
/// container, and nothing after it. It takes no record because there is none to
/// take, which is also why it stops where it does: the worktree path lived in the
/// record that went missing, and the record itself is already gone.
pub fn teardown_plan_without_record() -> Vec<TeardownStep> {
    stop_sequence()
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
        let plan = teardown_plan(&git_record());

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
    fn teardown_plan_stops_the_watcher_of_a_sandbox_that_never_had_one() {
        let plan = teardown_plan(&no_git_record());

        // Which host-side helpers a sandbox left running is the providers' answer
        // and not the plan's, exactly as the network step already covers pasta
        // alone and pasta with a proxy without the plan knowing which. Stopping
        // a watcher that was never started is not a failure, and a plan that
        // decided this for itself would need a record that remembers a pid, which
        // is precisely the state a restart makes meaningless.
        assert!(plan.contains(&TeardownStep::StopWatcher));
    }

    #[test]
    fn teardown_plan_without_record_stops_at_the_container() {
        let plan = teardown_plan_without_record();

        // A live anchor whose record is gone is collectable down to the
        // container and no further. The two steps it leaves out are the two that
        // would need the record: the worktree path is written there and nowhere
        // else, so removing one would mean removing a path hort cannot name, and
        // there is no record left to remove.
        assert_eq!(
            plan,
            vec![TeardownStep::StopWatcher, TeardownStep::StopNetwork, TeardownStep::StopContainer]
        );
    }

    #[test]
    fn rollback_plan_stops_at_the_container() {
        let plan = rollback_plan(&git_record());

        // The two steps it leaves out are exactly the two that touch the disk.
        // A failed build undoes what it started, never what holds work: the
        // worktree may carry changes from an earlier run, and the record is what
        // the next run reads to finish or clean this sandbox.
        assert_eq!(
            plan,
            vec![TeardownStep::StopWatcher, TeardownStep::StopNetwork, TeardownStep::StopContainer]
        );
    }
}
