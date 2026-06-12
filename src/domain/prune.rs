//! Prune selection: the pure decision of what `prune` removes, skips, or leaves
//! alone, over reconciled sandboxes and corrupt metadata dirs. Orphaned and
//! inconsistent sandboxes are debris and always candidates; a live sandbox is a
//! candidate only when an idle threshold is set and it has been idle at least
//! that long; an active or unknown-idle sandbox is never selected, and a lost
//! record is never prune's to remove. Every candidate, sandbox or corrupt dir
//! alike, is skipped when its worktree is dirty and `--force` was not passed.

use std::time::Duration;

use crate::domain::idle::IdleState;
use crate::domain::model::SandboxName;
use crate::domain::reconcile::SandboxState;

/// One reconciled sandbox as the selection sees it: its name, its cross-source
/// state, its idle (`None` when timestamps are unreadable, so never idle-selected),
/// and whether its worktree is dirty (`None` when there is nothing to protect).
pub struct PruneInput {
    pub name: SandboxName,
    pub state: SandboxState,
    pub idle: Option<IdleState>,
    pub dirty: Option<bool>,
}

/// One corrupt metadata dir as the selection sees it: its raw directory name and
/// whether its worktree is dirty.
pub struct CorruptInput {
    pub name: String,
    pub dirty: Option<bool>,
}

/// Why a candidate was skipped instead of removed. A closed set today; it grows
/// when live-project caches arrive.
#[derive(Debug, PartialEq)]
pub enum SkipReason {
    Dirty,
}

/// A candidate the selection chose not to remove, with the reason.
#[derive(Debug, PartialEq)]
pub struct PruneSkip {
    pub name: String,
    pub reason: SkipReason,
}

/// The selection's verdict: record-backed removals (executed via the teardown
/// plan), corrupt metadata dirs (executed in the fixed teardown order), and the
/// candidates skipped with their reason.
pub struct PrunePlan {
    pub sandboxes: Vec<SandboxName>,
    pub corrupt: Vec<String>,
    pub skipped: Vec<PruneSkip>,
}

/// Decide what `prune` removes, given the reconciled sandboxes, the corrupt
/// metadata dirs, the optional idle threshold, and whether `--force` was passed.
pub fn prune_selection(
    sandboxes: &[PruneInput],
    corrupt: &[CorruptInput],
    idle_threshold: Option<Duration>,
    force: bool,
) -> PrunePlan {
    let mut plan = PrunePlan { sandboxes: Vec::new(), corrupt: Vec::new(), skipped: Vec::new() };

    for input in sandboxes {
        if !is_candidate(input, idle_threshold) {
            continue;
        }
        if protected_by_dirty(input.dirty, force) {
            plan.skipped.push(skip(input.name.as_str()));
        } else {
            plan.sandboxes.push(input.name.clone());
        }
    }

    for input in corrupt {
        if protected_by_dirty(input.dirty, force) {
            plan.skipped.push(skip(&input.name));
        } else {
            plan.corrupt.push(input.name.clone());
        }
    }

    plan
}

/// Whether a reconciled sandbox is up for removal at all, before the dirty gate.
/// Debris (orphaned, inconsistent) always is; a live sandbox only when an idle
/// threshold is set and it has been idle at least that long; a lost record never
/// is, since adopt-or-clean is `ls`'s offer, not prune's.
fn is_candidate(input: &PruneInput, idle_threshold: Option<Duration>) -> bool {
    match input.state {
        SandboxState::Orphaned | SandboxState::Inconsistent => true,
        SandboxState::Live => idle_at_least(input.idle.as_ref(), idle_threshold),
        SandboxState::LostRecord => false,
    }
}

/// Whether the sandbox is idle and that idle time meets the threshold (inclusive
/// at equality). An active or unknown-idle sandbox, or an unset threshold, never
/// qualifies.
fn idle_at_least(idle: Option<&IdleState>, threshold: Option<Duration>) -> bool {
    matches!((idle, threshold), (Some(IdleState::Idle(elapsed)), Some(min)) if *elapsed >= min)
}

/// A dirty worktree is protected from removal unless `--force` was passed; the
/// gate guards uncommitted work and is the same for sandboxes and corrupt dirs.
fn protected_by_dirty(dirty: Option<bool>, force: bool) -> bool {
    dirty == Some(true) && !force
}

fn skip(name: &str) -> PruneSkip {
    PruneSkip { name: name.to_string(), reason: SkipReason::Dirty }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str) -> SandboxName {
        SandboxName::new(value).unwrap()
    }

    #[test]
    fn prune_selects_orphaned_sandbox() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], None, false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_inconsistent_sandbox() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Inconsistent,
            idle: None,
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], None, false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
    }

    #[test]
    fn prune_ignores_live_sandbox_without_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(86_400))),
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], None, false);

        assert!(plan.sandboxes.is_empty());
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_never_selects_lost_record() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::LostRecord,
            idle: None,
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_live_sandbox_idle_at_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(1800))),
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], Some(Duration::from_secs(1800)), false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
    }

    #[test]
    fn prune_ignores_live_sandbox_below_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(1799))),
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_ignores_active_sandbox_despite_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Active),
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], Some(Duration::from_secs(1800)), true);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_ignores_live_sandbox_with_unknown_idle() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: None,
            dirty: None,
        }];

        let plan = prune_selection(&inputs, &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_skips_dirty_sandbox_with_reason() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            dirty: Some(true),
        }];

        let plan = prune_selection(&inputs, &[], None, false);

        assert!(plan.sandboxes.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Dirty }]
        );
    }

    #[test]
    fn prune_force_includes_dirty_sandbox() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            dirty: Some(true),
        }];

        let plan = prune_selection(&inputs, &[], None, true);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_corrupt_entry() {
        let corrupt = vec![CorruptInput { name: "rotten".to_string(), dirty: None }];

        let plan = prune_selection(&[], &corrupt, None, false);

        assert_eq!(plan.corrupt, vec!["rotten".to_string()]);
    }

    #[test]
    fn prune_skips_dirty_corrupt_entry() {
        let corrupt = vec![CorruptInput { name: "rotten".to_string(), dirty: Some(true) }];

        let plan = prune_selection(&[], &corrupt, None, false);

        assert!(plan.corrupt.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip { name: "rotten".to_string(), reason: SkipReason::Dirty }]
        );
    }
}
