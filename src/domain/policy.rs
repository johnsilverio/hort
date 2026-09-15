//! Command-admission policy: pure error selection over observed sandbox state.
//! `up` asks two questions, is the same-named anchor alive and is the network
//! it stands on still there, and decides which error to raise (if any) before
//! doing any work. The selection is a pure function of the reconciled state,
//! the network fact and the branch intent; the command runs the effects.

use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName};
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
