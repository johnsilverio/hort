//! `HortError`: the domain error type. User-facing variants carry the runtime
//! placeholders for their **verbatim canonical message**; `Display` renders that
//! message and `exit_code()` maps it to a process exit code (hand-written, no
//! `thiserror`). `InvalidName` is the unit validation signal from the newtypes.

use std::fmt;

/// Domain error type.
///
/// Each user-facing variant carries the runtime values its canonical message
/// interpolates; the message itself is produced by `Display`. `InvalidName` is
/// the signal a validated newtype (`SandboxName`/`BranchName`/`Domain`) returns
/// when it rejects its input, matched as a **unit** variant.
#[derive(Debug, PartialEq, Eq)]
pub enum HortError {
    /// A validated domain newtype rejected its input. Matched as a **unit**
    /// variant (`HortError::InvalidName`), never `HortError::InvalidName { .. }`.
    InvalidName,
    /// `up`: a fully built sandbox of this name already exists.
    DuplicateName { name: String },
    /// `up`: the new branch named after the sandbox already exists.
    BranchExists { name: String },
    /// `up`: the target branch is already checked out in another worktree.
    BranchCheckedOut { branch: String },
    /// `up`: `--branch` named a branch that does not exist.
    BranchDoesNotExist { branch: String, name: String },
    /// `up`: another invocation for this name is already in progress.
    UpInProgress { name: String },
    /// `up`: a branch flag was given in a project that is not a git repository.
    BranchRequiresGit,
    /// `attach`: the name has metadata but no live anchor.
    SandboxNotRunning { name: String },
    /// `attach`: no sandbox of this name is known ("what's alive" wording).
    UnknownSandboxOnAttach { name: String },
    /// `down`: no sandbox of this name is known ("what exists" wording).
    UnknownSandboxOnDown { name: String },
    /// `down`/`prune`: confirmation was required but stdin is not a TTY and
    /// `--force` was not passed. The `command` placeholder renders as "down" or
    /// "prune".
    RefusedWithoutConfirmation { command: String },
    /// Config parsing failed: the input was not valid JSONC. Carries a
    /// human-readable detail; the rendered message is not a canonical product
    /// string, so callers match the variant, not the text.
    InvalidConfig { detail: String },
    /// A persisted timestamp could not be parsed as strict RFC 3339 UTC. Carries
    /// a human-readable detail; the rendered message is not a canonical product
    /// string, so callers match the variant, not the text.
    InvalidTimestamp { detail: String },
    /// On-disk metadata could not be read as a valid record: unreadable JSON, an
    /// invalid name, an unparseable timestamp, or an unknown schema version.
    /// Carries a human-readable detail; the rendered message is not a canonical
    /// product string, so callers match the variant, not the text.
    CorruptMetadata { detail: String },
    /// A git invocation in the worktree adapter failed. Carries a human-readable
    /// detail; the rendered message is not a canonical product string, so callers
    /// match the variant, not the text.
    GitCommandFailed { detail: String },
}

impl HortError {
    /// The non-zero process exit code this error maps to, printed once at the
    /// `main.rs` boundary.
    pub fn exit_code(&self) -> u8 {
        1
    }
}

impl fmt::Display for HortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HortError::InvalidName => write!(f, "invalid name"),
            HortError::DuplicateName { name } => write!(
                f,
                "a sandbox named '{name}' already exists (run 'hort attach {name}' to join it, or 'hort down {name}' first)"
            ),
            HortError::BranchExists { name } => write!(
                f,
                "branch '{name}' already exists; choose another name or use --branch to target an existing branch"
            ),
            HortError::BranchCheckedOut { branch } => write!(
                f,
                "branch '{branch}' is already checked out in another worktree"
            ),
            HortError::BranchDoesNotExist { branch, name } => write!(
                f,
                "branch '{branch}' does not exist; create it first or omit --branch to create a new branch named '{name}'"
            ),
            HortError::UpInProgress { name } => {
                write!(f, "another 'hort up {name}' is already in progress")
            }
            HortError::BranchRequiresGit => write!(
                f,
                "--branch requires a git repository, but this project is not one"
            ),
            HortError::SandboxNotRunning { name } => write!(
                f,
                "sandbox '{name}' is not running (run 'hort up {name}' to start it, or 'hort prune' to clean up the stale record)"
            ),
            HortError::UnknownSandboxOnAttach { name } => write!(
                f,
                "no sandbox named '{name}' (run 'hort ls' to see what's alive)"
            ),
            HortError::UnknownSandboxOnDown { name } => write!(
                f,
                "no sandbox named '{name}' (run 'hort ls' to see what exists)"
            ),
            HortError::RefusedWithoutConfirmation { command } => write!(
                f,
                "refusing to {command} without confirmation: stdin is not a TTY (pass --force to proceed)"
            ),
            HortError::InvalidConfig { detail } => write!(f, "invalid config: {detail}"),
            HortError::InvalidTimestamp { detail } => write!(f, "invalid timestamp: {detail}"),
            HortError::CorruptMetadata { detail } => write!(f, "corrupt metadata: {detail}"),
            HortError::GitCommandFailed { detail } => write!(f, "git command failed: {detail}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_name_error_renders_canonical_string() {
        let error = HortError::DuplicateName { name: "demo".to_string() };

        assert_eq!(
            error.to_string(),
            "a sandbox named 'demo' already exists (run 'hort attach demo' to join it, or 'hort down demo' first)"
        );
    }

    #[test]
    fn not_running_differs_from_absent_message() {
        let not_running = HortError::SandboxNotRunning { name: "demo".to_string() };
        let absent = HortError::UnknownSandboxOnAttach { name: "demo".to_string() };

        assert_eq!(
            not_running.to_string(),
            "sandbox 'demo' is not running (run 'hort up demo' to start it, or 'hort prune' to clean up the stale record)"
        );
        assert_eq!(
            absent.to_string(),
            "no sandbox named 'demo' (run 'hort ls' to see what's alive)"
        );
    }

    #[test]
    fn down_unknown_name_uses_what_exists_wording() {
        let down = HortError::UnknownSandboxOnDown { name: "demo".to_string() };
        let attach = HortError::UnknownSandboxOnAttach { name: "demo".to_string() };

        assert_eq!(
            down.to_string(),
            "no sandbox named 'demo' (run 'hort ls' to see what exists)"
        );
        assert_ne!(down.to_string(), attach.to_string());
    }

    #[test]
    fn branch_exists_error_renders_canonical_string() {
        let error = HortError::BranchExists { name: "demo".to_string() };

        assert_eq!(
            error.to_string(),
            "branch 'demo' already exists; choose another name or use --branch to target an existing branch"
        );
    }

    #[test]
    fn branch_checked_out_error_renders_canonical_string() {
        let error = HortError::BranchCheckedOut { branch: "feature-x".to_string() };

        assert_eq!(
            error.to_string(),
            "branch 'feature-x' is already checked out in another worktree"
        );
    }

    #[test]
    fn branch_does_not_exist_error_renders_canonical_string() {
        let error = HortError::BranchDoesNotExist {
            branch: "feature-x".to_string(),
            name: "demo".to_string(),
        };

        assert_eq!(
            error.to_string(),
            "branch 'feature-x' does not exist; create it first or omit --branch to create a new branch named 'demo'"
        );
    }

    #[test]
    fn up_in_progress_error_renders_canonical_string() {
        let error = HortError::UpInProgress { name: "demo".to_string() };

        assert_eq!(
            error.to_string(),
            "another 'hort up demo' is already in progress"
        );
    }

    #[test]
    fn branch_requires_git_error_renders_canonical_string() {
        let error = HortError::BranchRequiresGit;

        assert_eq!(
            error.to_string(),
            "--branch requires a git repository, but this project is not one"
        );
    }

    #[test]
    fn refused_without_confirmation_error_renders_canonical_string() {
        let error = HortError::RefusedWithoutConfirmation { command: "down".to_string() };

        assert_eq!(
            error.to_string(),
            "refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)"
        );
    }

    #[test]
    fn error_exit_code_is_nonzero() {
        let error = HortError::DuplicateName { name: "demo".to_string() };

        assert_ne!(error.exit_code(), 0);
    }
}
