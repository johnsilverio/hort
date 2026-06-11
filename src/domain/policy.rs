//! Command-admission policy: pure error selection over observed sandbox state.
//! Both `up` and `attach` ask one question, is the same-named anchor alive, and
//! decide which error to raise (if any) before doing any work. The selection is a
//! pure function of the reconciled state and the branch intent; the commands run
//! the effects.

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
/// proceed. Checks run in precedence order: a held lock first, then a colliding
/// live sandbox of the same name, then the branch target.
pub fn up_error(
    name: &SandboxName,
    lock_held: bool,
    existing: Option<SandboxState>,
    branch: BranchIntent,
) -> Option<HortError> {
    if lock_held {
        return Some(HortError::UpInProgress { name: name.as_str().to_string() });
    }

    if let Some(SandboxState::Live | SandboxState::LostRecord | SandboxState::Inconsistent) = existing {
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

/// Select the error `attach` must raise, or `None` to join. A live sandbox is
/// joinable; an orphaned record is not running; an unknown name has nothing to
/// join.
pub fn attach_error(name: &SandboxName, existing: Option<SandboxState>) -> Option<HortError> {
    match existing {
        None => Some(HortError::UnknownSandboxOnAttach { name: name.as_str().to_string() }),
        Some(SandboxState::Orphaned) => {
            Some(HortError::SandboxNotRunning { name: name.as_str().to_string() })
        }
        Some(SandboxState::Live | SandboxState::LostRecord | SandboxState::Inconsistent) => None,
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
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_no_error_when_name_and_branch_are_free() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            None,
            BranchIntent::CreateNew { branch_taken: false },
        );

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_branch_exists_for_new_branch_collision() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            false,
            None,
            BranchIntent::CreateNew { branch_taken: true },
        );

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
            BranchIntent::UseExisting { branch, checked_out_elsewhere: true },
        );

        assert_eq!(error, Some(HortError::BranchCheckedOut { branch: "feature-x".to_string() }));
    }

    #[test]
    fn up_selects_branch_requires_git_for_branch_flag_without_git() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(&name, false, None, BranchIntent::NoGit { branch_flag: true });

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
            BranchIntent::UseExisting { branch, checked_out_elsewhere: false },
        );

        assert_eq!(error, None);
    }

    #[test]
    fn up_selects_no_error_in_no_git_without_branch_flag() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(&name, false, None, BranchIntent::NoGit { branch_flag: false });

        assert_eq!(error, None);
    }

    #[test]
    fn up_lock_takes_precedence_over_duplicate_name() {
        let name = SandboxName::new("demo").unwrap();

        let error = up_error(
            &name,
            true,
            Some(SandboxState::Live),
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
            BranchIntent::CreateNew { branch_taken: true },
        );

        assert_eq!(error, Some(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn attach_selects_not_running_for_orphaned_record() {
        let name = SandboxName::new("demo").unwrap();

        let error = attach_error(&name, Some(SandboxState::Orphaned));

        assert_eq!(error, Some(HortError::SandboxNotRunning { name: "demo".to_string() }));
    }

    #[test]
    fn attach_selects_absent_for_unknown_name() {
        let name = SandboxName::new("demo").unwrap();

        let error = attach_error(&name, None);

        assert_eq!(error, Some(HortError::UnknownSandboxOnAttach { name: "demo".to_string() }));
    }

    #[test]
    fn attach_selects_no_error_for_live_sandbox() {
        let name = SandboxName::new("demo").unwrap();

        let error = attach_error(&name, Some(SandboxState::Live));

        assert_eq!(error, None);
    }
}
