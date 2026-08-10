//! Prune selection: the pure decision of what `prune` removes, skips, or leaves
//! alone, over reconciled sandboxes, corrupt metadata dirs and stored caches.
//! Orphaned and inconsistent sandboxes are debris and always candidates; a live
//! sandbox is a candidate only when an idle threshold is set and it has been idle
//! at least that long; an active or unknown-idle sandbox is never selected, and a
//! lost record is never prune's to remove. Every candidate then passes a
//! fail-closed guard: without `--force` only a worktree proven to hold nothing
//! and a cache whose project is proven gone are removed, and one hort could not
//! read is skipped with a reason of its own. A cache answers one thing before any
//! of that, and `--force` is not passed to it: a directory a running container is
//! standing on is not a candidate at all.

use std::time::Duration;

use crate::domain::cache::project_from_cache_key;
use crate::domain::idle::IdleState;
use crate::domain::model::SandboxName;
use crate::domain::reconcile::SandboxState;

/// What a candidate's worktree holds, as far as hort was able to determine.
///
/// The three states are deliberately distinct. A single value carrying both
/// "nothing at risk" and "could not tell" leaves the gate to resolve the
/// ambiguity, and a gate resolves it toward removal, which is the direction that
/// costs a user their uncommitted work.
pub enum WorktreeRisk {
    HoldsWork,
    NothingAtRisk,
    Unknown,
}

/// One reconciled sandbox as the selection sees it: its name, its cross-source
/// state, its idle (`None` when timestamps are unreadable, so never idle-selected),
/// and what its worktree holds.
pub struct PruneInput {
    pub name: SandboxName,
    pub state: SandboxState,
    pub idle: Option<IdleState>,
    pub risk: WorktreeRisk,
}

/// One corrupt metadata dir as the selection sees it: its raw directory name and
/// what its worktree holds.
pub struct CorruptInput {
    pub name: String,
    pub risk: WorktreeRisk,
}

/// Whether the project a stored cache belongs to is still there, as far as hort
/// was able to determine.
///
/// It mirrors `WorktreeRisk` on purpose, so the same reading serves both, and it
/// names the question rather than one of its own answers. `ProjectLives` is the
/// protected state: a cache is only worth removing once the project that fills
/// it is gone.
pub enum CacheRisk {
    ProjectLives,
    NothingAtRisk,
    Unknown,
}

/// Whether a running container is standing on a stored cache right now, as far
/// as hort was able to place the live sandboxes on the machine.
///
/// The question is live and never active: what holds the bind is the container,
/// and the container stands as long as its anchor does, with or without a session
/// running inside it.
pub enum CacheHold {
    HeldByLiveSandbox,
    NoLiveSandbox,
    Unknown,
}

/// One stored cache as the selection sees it: the on-disk directory name it is
/// addressed by, whether the project it belongs to is still there, and whether a
/// live sandbox is standing on it.
///
/// Removal is by whole directory rather than by entry, because listing a
/// project's entries needs that project's configuration, and the configuration of
/// a project that no longer exists cannot be read at all.
pub struct CacheInput {
    pub key: String,
    pub risk: CacheRisk,
    pub hold: CacheHold,
}

/// Why a candidate was skipped instead of removed.
///
/// `Unknown` is not cosmetic: reporting a box as dirty when its repository is
/// gone sends the user hunting for uncommitted changes git can no longer
/// enumerate, and that report is what they read before deciding to force. The
/// cache reasons are separate from it for the same reason they exist at all: one
/// word for both questions sends that reader to the wrong place.
#[derive(Debug, PartialEq)]
pub enum SkipReason {
    Dirty,
    Unknown,
    LiveProject,
    UnknownProject,
    LiveSandbox,
    UnknownSandbox,
}

/// A candidate the selection chose not to remove, with the reason.
#[derive(Debug, PartialEq)]
pub struct PruneSkip {
    pub name: String,
    pub reason: SkipReason,
}

/// The selection's verdict: record-backed removals (executed via the teardown
/// plan), corrupt metadata dirs (executed in the fixed teardown order), the keys
/// of the caches to collect, and the candidates skipped with their reason.
pub struct PrunePlan {
    pub sandboxes: Vec<SandboxName>,
    pub corrupt: Vec<String>,
    pub caches: Vec<String>,
    pub skipped: Vec<PruneSkip>,
}

/// Decide what `prune` removes, given the reconciled sandboxes, the corrupt
/// metadata dirs, the stored caches, the optional idle threshold, and whether
/// `--force` was passed.
pub fn prune_selection(
    sandboxes: &[PruneInput],
    corrupt: &[CorruptInput],
    caches: &[CacheInput],
    idle_threshold: Option<Duration>,
    force: bool,
) -> PrunePlan {
    let mut plan = PrunePlan {
        sandboxes: Vec::new(),
        corrupt: Vec::new(),
        caches: Vec::new(),
        skipped: Vec::new(),
    };

    for input in sandboxes {
        if !is_candidate(input, idle_threshold) {
            continue;
        }
        match protected(&input.risk, force) {
            Some(reason) => plan.skipped.push(skip(input.name.as_str(), reason)),
            None => plan.sandboxes.push(input.name.clone()),
        }
    }

    for input in corrupt {
        match protected(&input.risk, force) {
            Some(reason) => plan.skipped.push(skip(&input.name, reason)),
            None => plan.corrupt.push(input.name.clone()),
        }
    }

    for input in caches {
        // Physics before value: a cache a container is standing on is not a
        // candidate at all, so the force the gate below reads never reaches it.
        let held_back =
            cache_candidate(&input.hold).or_else(|| cache_protected(&input.risk, force));
        match held_back {
            // A key is the address hort stores a cache under; the project is what
            // the user recognizes as theirs, and this line is the last one read
            // before deciding whether to force.
            Some(reason) => {
                let project = project_from_cache_key(&input.key);
                plan.skipped.push(skip(&project.display().to_string(), reason));
            }
            None => plan.caches.push(input.key.clone()),
        }
    }

    plan
}

/// Whether a reconciled sandbox is up for removal at all, before the risk guard.
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

/// Why a candidate is held back, or `None` when it may be removed. The gate is
/// the same for sandboxes and corrupt dirs, and it decides by permission: only a
/// worktree hort proved holds nothing leaves without `--force`, so a state it
/// could not read protects instead of deleting. Failing that way round costs a
/// run of `prune` that removed less than it could; failing the other way costs
/// work that was never committed anywhere else.
fn protected(risk: &WorktreeRisk, force: bool) -> Option<SkipReason> {
    if force {
        return None;
    }
    match risk {
        WorktreeRisk::HoldsWork => Some(SkipReason::Dirty),
        WorktreeRisk::Unknown => Some(SkipReason::Unknown),
        WorktreeRisk::NothingAtRisk => None,
    }
}

/// Why a stored cache is not up for collection at all, or `None` when the
/// question of value below may be asked about it.
///
/// It takes no `force`, and that absence is the guarantee: the flag buys value,
/// never physics. A cache is a writable bind source, and unlinking one while a
/// container is standing on it leaves that container reading and writing into
/// nothing. Whoever wants the directory downs the box first, and the next run
/// collects it. Unlike the sandbox arm, a cache held back here still reports the
/// skip, because prune's default selection does include orphaned caches: one that
/// quietly failed to go would read as a bug and send the user reaching for
/// `--force`, which is the one thing that cannot help.
pub fn cache_candidate(hold: &CacheHold) -> Option<SkipReason> {
    match hold {
        CacheHold::HeldByLiveSandbox => Some(SkipReason::LiveSandbox),
        CacheHold::Unknown => Some(SkipReason::UnknownSandbox),
        CacheHold::NoLiveSandbox => None,
    }
}

/// Why a stored cache is held back, or `None` when it may be collected. Same
/// permission gate as the worktrees, asked of a different question: only a
/// project hort proved is gone from disk releases its cache without `--force`,
/// and a presence read that failed protects. Failing that way round leaves a
/// directory behind; failing the other way collects the cache of every project on
/// a machine where that read happens to be failing.
fn cache_protected(risk: &CacheRisk, force: bool) -> Option<SkipReason> {
    if force {
        return None;
    }
    match risk {
        CacheRisk::ProjectLives => Some(SkipReason::LiveProject),
        CacheRisk::Unknown => Some(SkipReason::UnknownProject),
        CacheRisk::NothingAtRisk => None,
    }
}

fn skip(name: &str, reason: SkipReason) -> PruneSkip {
    PruneSkip { name: name.to_string(), reason }
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
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_inconsistent_sandbox() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Inconsistent,
            idle: None,
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
    }

    #[test]
    fn prune_ignores_live_sandbox_without_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(86_400))),
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, false);

        assert!(plan.sandboxes.is_empty());
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_never_selects_lost_record() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::LostRecord,
            idle: None,
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_live_sandbox_idle_at_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(1800))),
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], Some(Duration::from_secs(1800)), false);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
    }

    #[test]
    fn prune_ignores_live_sandbox_below_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Idle(Duration::from_secs(1799))),
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_ignores_active_sandbox_despite_idle_threshold() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: Some(IdleState::Active),
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], Some(Duration::from_secs(1800)), true);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_ignores_live_sandbox_with_unknown_idle() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Live,
            idle: None,
            risk: WorktreeRisk::NothingAtRisk,
        }];

        let plan = prune_selection(&inputs, &[], &[], Some(Duration::from_secs(1800)), false);

        assert!(plan.sandboxes.is_empty());
    }

    #[test]
    fn prune_skips_dirty_sandbox_with_reason() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            risk: WorktreeRisk::HoldsWork,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, false);

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
            risk: WorktreeRisk::HoldsWork,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, true);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_selects_corrupt_entry() {
        let corrupt =
            vec![CorruptInput { name: "rotten".to_string(), risk: WorktreeRisk::NothingAtRisk }];

        let plan = prune_selection(&[], &corrupt, &[], None, false);

        assert_eq!(plan.corrupt, vec!["rotten".to_string()]);
    }

    #[test]
    fn prune_skips_a_candidate_whose_worktree_state_is_unknown() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            risk: WorktreeRisk::Unknown,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, false);

        // Not knowing what a worktree holds is not evidence that it holds
        // nothing, and the guard exists to protect uncommitted work.
        assert!(plan.sandboxes.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }]
        );
    }

    #[test]
    fn prune_force_includes_a_candidate_whose_worktree_state_is_unknown() {
        let inputs = vec![PruneInput {
            name: name("demo"),
            state: SandboxState::Orphaned,
            idle: None,
            risk: WorktreeRisk::Unknown,
        }];

        let plan = prune_selection(&inputs, &[], &[], None, true);

        assert_eq!(plan.sandboxes, vec![name("demo")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_skips_dirty_corrupt_entry() {
        let corrupt =
            vec![CorruptInput { name: "rotten".to_string(), risk: WorktreeRisk::HoldsWork }];

        let plan = prune_selection(&[], &corrupt, &[], None, false);

        assert!(plan.corrupt.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip { name: "rotten".to_string(), reason: SkipReason::Dirty }]
        );
    }

    #[test]
    fn prune_selects_a_cache_whose_project_is_gone() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fgone".to_string(),
            risk: CacheRisk::NothingAtRisk,
            hold: CacheHold::NoLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, false);

        // The whole reason the key is a reversible encoding and not a hash: a
        // stored cache can say which project filled it, and a project that is
        // gone from disk will never fill it again.
        assert_eq!(plan.caches, vec!["%2Fhome%2Ftester%2Fprojects%2Fgone".to_string()]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_skips_a_cache_of_a_live_project_with_reason() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fhort".to_string(),
            risk: CacheRisk::ProjectLives,
            hold: CacheHold::NoLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, false);

        // The line a skip produces is the last thing read before deciding to
        // pass --force, so it names the project the user recognizes and not the
        // address hort invented to store it under.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/hort".to_string(),
                reason: SkipReason::LiveProject,
            }]
        );
    }

    #[test]
    fn prune_skips_a_cache_whose_project_cannot_be_read() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fhort".to_string(),
            risk: CacheRisk::Unknown,
            hold: CacheHold::NoLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, false);

        // Printing the worktree word here would send the user looking for
        // uncommitted changes in a directory that holds none by construction,
        // which is the wrong place to look right before deciding to force.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/hort".to_string(),
                reason: SkipReason::UnknownProject,
            }]
        );
    }

    #[test]
    fn prune_force_includes_a_cache_of_a_live_project() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fhort".to_string(),
            risk: CacheRisk::ProjectLives,
            hold: CacheHold::NoLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, true);

        assert_eq!(plan.caches, vec!["%2Fhome%2Ftester%2Fprojects%2Fhort".to_string()]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn prune_spares_a_cache_a_live_sandbox_holds() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fgone".to_string(),
            risk: CacheRisk::NothingAtRisk,
            hold: CacheHold::HeldByLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, false);

        // Everything about the project says collect it: it is gone from disk and
        // will never fill this directory again. What forbids it is the container
        // still standing on the directory, which is a writable bind source and
        // stops answering reads and writes the moment the host unlinks it.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/gone".to_string(),
                reason: SkipReason::LiveSandbox,
            }]
        );
    }

    #[test]
    fn prune_force_does_not_take_a_cache_a_live_sandbox_holds() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fhort".to_string(),
            risk: CacheRisk::ProjectLives,
            hold: CacheHold::HeldByLiveSandbox,
        }];

        let plan = prune_selection(&[], &[], &caches, None, true);

        // The flag buys value: it says spend the four minutes of installing
        // again. It cannot buy physics. A running box holding this directory
        // survives the run and watches its own writable mount go, which is the
        // one thing the teardown order exists to forbid.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/hort".to_string(),
                reason: SkipReason::LiveSandbox,
            }]
        );
    }

    #[test]
    fn prune_spares_a_cache_when_it_cannot_place_a_live_sandbox() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fgone".to_string(),
            risk: CacheRisk::NothingAtRisk,
            hold: CacheHold::Unknown,
        }];

        let plan = prune_selection(&[], &[], &caches, None, false);

        // An anchor hort cannot place might be standing on any cache on the
        // machine, so not knowing who holds this one is not evidence that
        // nobody does.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/gone".to_string(),
                reason: SkipReason::UnknownSandbox,
            }]
        );
    }

    #[test]
    fn prune_force_does_not_take_a_cache_it_cannot_place() {
        let caches = vec![CacheInput {
            key: "%2Fhome%2Ftester%2Fprojects%2Fgone".to_string(),
            risk: CacheRisk::NothingAtRisk,
            hold: CacheHold::Unknown,
        }];

        let plan = prune_selection(&[], &[], &caches, None, true);

        // A project hort could not read is released by --force, because the
        // question there is only whether the user still wants what is stored. A
        // sandbox hort could not place is not, because the answer decides
        // whether a process is standing on the directory right now.
        assert!(plan.caches.is_empty());
        assert_eq!(
            plan.skipped,
            vec![PruneSkip {
                name: "/home/tester/projects/gone".to_string(),
                reason: SkipReason::UnknownSandbox,
            }]
        );
    }
}
