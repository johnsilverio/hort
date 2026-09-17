//! Command-admission policy: pure error selection over observed sandbox state.
//! `up` asks two questions, is the same-named anchor alive and is the network
//! it stands on still there, and decides which error to raise (if any) before
//! doing any work. The selection is a pure function of the reconciled state,
//! the network fact and the branch intent; the command runs the effects.

use crate::domain::config::GitMode;
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName, Warning};
use crate::domain::reconcile::SandboxState;

/// What `up` intends to do about the branch, observed before any git call. Each
/// variant carries the one fact the selection needs, so an illegal combination
/// cannot be represented.
pub enum BranchIntent {
    /// Default mode: hort creates a new branch named after the sandbox.
    /// `branch_taken` says whether that branch name already exists.
    CreateNew { branch_taken: bool },
    /// The user asked to target an existing branch. `checked_out_elsewhere` says
    /// whether that branch is already checked out in another worktree.
    UseExisting { branch: BranchName, checked_out_elsewhere: bool },
    /// The project is not a git repository. `branch_flag` says whether the user
    /// passed a branch flag, which has no meaning without git.
    NoGit { branch_flag: bool },
}

/// Select the error `up` must raise before building the sandbox, or `None` to
/// proceed. Checks run in precedence order: a held lock first, then a same-named
/// sandbox that is already whole, then the branch target.
///
/// A live sandbox is whole only while its network is standing. One whose anchor
/// is up and whose helpers are not is half-built, whether the `up` that made it
/// died before wiring it or a helper died under it, and the kernel's liveness
/// answer alone cannot tell the two apart. Such a run goes on and finishes the
/// network. A lost or inconsistent record is refused whatever the network says:
/// the first hort cannot rebuild from what it has, and the second is missing its
/// worktree, which no provisioning brings back.
pub fn up_error(
    name: &SandboxName,
    lock_held: bool,
    existing: Option<SandboxState>,
    network_standing: bool,
    branch: BranchIntent,
) -> Option<HortError> {
    if lock_held {
        return Some(HortError::UpInProgress { name: name.as_str().to_string() });
    }

    let whole = match existing {
        Some(SandboxState::Live) => network_standing,
        Some(SandboxState::LostRecord | SandboxState::Inconsistent) => true,
        Some(SandboxState::Orphaned) | None => false,
    };
    if whole {
        return Some(HortError::DuplicateName { name: name.as_str().to_string() });
    }

    match branch {
        BranchIntent::CreateNew { branch_taken: true } => {
            Some(HortError::BranchExists { name: name.as_str().to_string() })
        }
        BranchIntent::CreateNew { branch_taken: false } => None,
        BranchIntent::UseExisting { branch, checked_out_elsewhere: true } => {
            Some(HortError::BranchCheckedOut { branch: branch.as_str().to_string() })
        }
        BranchIntent::UseExisting { checked_out_elsewhere: false, .. } => None,
        BranchIntent::NoGit { branch_flag: true } => Some(HortError::BranchRequiresGit),
        BranchIntent::NoGit { branch_flag: false } => None,
    }
}

/// Decide how `up` will get git into `/workdir`, and what it has to say about
/// that choice. The flag the user typed wins over the merged configuration, and
/// nothing declared anywhere leaves the worktree mode every sandbox was built
/// with until now.
///
/// A clone asked of a directory holding no repository is two different
/// situations. Typed as a flag it is a request for this one build that hort
/// cannot honour, so it is refused. Carried by a configuration layer it is a
/// default that does not fit here: the global layer covers every directory on
/// the machine, and refusing there would break `up` in every folder that is not
/// a repository, so it warns and the sandbox is built without git.
pub fn resolve_git_mode(
    flag: Option<GitMode>,
    configured: Option<GitMode>,
    is_git_repo: bool,
) -> Result<(GitMode, Option<Warning>), HortError> {
    let requested = flag.or(configured).unwrap_or(GitMode::Worktree);
    let clone_without_a_repository = requested == GitMode::Clone && !is_git_repo;
    if !clone_without_a_repository {
        return Ok((requested, None));
    }
    if flag.is_some() {
        return Err(HortError::CloneRequiresGit);
    }
    Ok((
        GitMode::Worktree,
        Some(Warning::new(
            "the configured 'clone' git mode needs a git repository and this project is not one, so the sandbox mounts the project folder itself",
        )),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_selects_duplicate_name_for_built_live_sandbox() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            Some(SandboxState::Live),
            true,
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, Some(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn up_selects_duplicate_name_for_live_anchor_without_record() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            Some(SandboxState::LostRecord),
            true,
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, Some(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn up_selects_no_error_for_orphaned_record() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            Some(SandboxState::Orphaned),
            true,
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_no_error_when_name_and_branch_are_free() {
        let name = SandboxName::new("demo").unwrap();

        let error =
            up_error(&name, false, None, true, BranchIntent::CreateNew { branch_taken: false });

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_branch_exists_for_new_branch_collision() {
        let name = SandboxName::new("demo").unwrap();

        let error =
            up_error(&name, false, None, true, BranchIntent::CreateNew { branch_taken: true });

        assert_eq!(error, Some(HortError::BranchExists { name: "demo".to_string() }));
    }

    #[test]
    fn up_selects_branch_checked_out_for_existing_branch_in_use() {
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("feature-x").unwrap();

        let error = up_error(
            &name,
            false,
            None,
            true,
            BranchIntent::UseExisting { branch, checked_out_elsewhere: true },
        );

        assert_eq!(error, Some(HortError::BranchCheckedOut { branch: "feature-x".to_string() }));
    }

    #[test]
    fn up_selects_branch_requires_git_for_branch_flag_without_git() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(&name, false, None, true, BranchIntent::NoGit { branch_flag: true });

        assert_eq!(error, Some(HortError::BranchRequiresGit));
    }

    #[test]
    fn up_selects_no_error_for_free_existing_branch() {
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("feature-x").unwrap();

        let error = up_error(
            &name,
            false,
            None,
            true,
            BranchIntent::UseExisting { branch, checked_out_elsewhere: false },
        );

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_no_error_in_no_git_without_branch_flag() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(&name, false, None, true, BranchIntent::NoGit { branch_flag: false });

        assert_eq!(error, None);
    }

    #[test]
    fn git_mode_flag_wins_over_the_configured_mode() {
        let resolved = resolve_git_mode(Some(GitMode::Worktree), Some(GitMode::Clone), true);

        assert_eq!(resolved, Ok((GitMode::Worktree, None)));
    }

    #[test]
    fn git_mode_falls_back_to_worktree_when_no_layer_declares_one() {
        let resolved = resolve_git_mode(None, None, true);

        assert_eq!(resolved, Ok((GitMode::Worktree, None)));
    }

    #[test]
    fn clone_asked_by_the_flag_is_refused_without_git() {
        let resolved = resolve_git_mode(Some(GitMode::Clone), None, false);

        assert_eq!(resolved, Err(HortError::CloneRequiresGit));
    }

    #[test]
    fn clone_asked_by_the_configuration_degrades_without_git() {
        // The layer that carries this mode covers every directory on the
        // machine, so refusing here would break `up` wherever the user is not
        // standing in a repository.
        let (_mode, warning) = resolve_git_mode(None, Some(GitMode::Clone), false)
            .expect("a configured clone mode must not refuse a project without git");

        assert!(warning.is_some());
    }

    #[test]
    fn clone_degraded_without_git_resolves_to_the_worktree_mode() {
        // What the degradation answers with is a second guarantee: a mode that
        // still said clone here would send the build looking for a repository
        // to clone that this directory does not have.
        let (mode, _warning) = resolve_git_mode(None, Some(GitMode::Clone), false)
            .expect("a configured clone mode must not refuse a project without git");

        assert_eq!(mode, GitMode::Worktree);
    }

    #[test]
    fn up_lock_takes_precedence_over_duplicate_name() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            true,
            Some(SandboxState::Live),
            true,
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, Some(HortError::UpInProgress { name: "demo".to_string() }));
    }

    #[test]
    fn up_selects_duplicate_name_over_branch_collision() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            Some(SandboxState::Live),
            true,
            BranchIntent::CreateNew { branch_taken: true },
        );

        assert_eq!(error, Some(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn up_proceeds_when_a_live_sandbox_has_no_network_standing() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            Some(SandboxState::Live),
            false,
            BranchIntent::CreateNew { branch_taken: false },
        );

        // The anchor stands and nothing carries its traffic: the box is
        // half-built, which is what reentrancy exists to finish. Refused as a
        // duplicate, it stays a live box with no route that only `down` can
        // reach, and nothing else on the machine can tell it from a healthy one.
        assert_eq!(error, None);
    }
}
