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
    /// `up`/`attach`: the kernel has unprivileged user namespaces disabled, so no
    /// sandbox can be built at all.
    UserNamespacesDisabled,
    /// `up`/`attach`: the pasta binary is not on `PATH`.
    PastaMissing,
    /// `up`: no rootfs directory is resolvable from the merged config.
    NoRootfsConfigured,
    /// `up`: the configured rootfs directory does not exist on the host.
    RootfsMissing { path: String },
    /// `up`: the rootfs provides no default shell to run the anchor and sessions.
    RootfsWithoutShell { path: String },
    /// `up`: the `shell` the config declares is absent from the rootfs.
    ShellNotInRootfs { shell: String, path: String },
    /// `up`: `/workdir` in the rootfs is not writable by the uid the sandbox maps
    /// the in-container user to.
    WorkdirNotWritable { path: String },
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
    /// A container-runtime operation failed: starting the anchor, joining a
    /// session, or tearing the container down. Carries a human-readable detail
    /// naming the operation; the rendered message is not a canonical product
    /// string, so callers match the variant, not the text.
    ContainerRuntimeFailed { detail: String },
    /// The container runtime is not available in this build. The placeholder
    /// runtime adapter returns this for any operation that would start a real
    /// container; it disappears once the embedded runtime lands. Its message is
    /// not a canonical product string.
    RuntimeUnavailable,
    /// Preparing the state directory failed: creating or resolving the root hort
    /// keeps its records under. Carries a human-readable detail; the rendered
    /// message is not a canonical product string, so callers match the variant,
    /// not the text.
    StateIo { detail: String },
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
            HortError::UserNamespacesDisabled => write!(
                f,
                "unprivileged user namespaces are disabled in this kernel — hort cannot create a sandbox"
            ),
            HortError::PastaMissing => {
                write!(f, "pasta not found on PATH — hort needs it for sandbox networking")
            }
            HortError::NoRootfsConfigured => write!(
                f,
                "no rootfs configured — set \"rootfs\" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json"
            ),
            HortError::RootfsMissing { path } => write!(
                f,
                "rootfs directory '{path}' does not exist — prepare it first with podman export, debootstrap or umoci unpack"
            ),
            HortError::RootfsWithoutShell { path } => write!(
                f,
                "rootfs '{path}' has no usable shell (expected /bin/sh) — the rootfs must provide one"
            ),
            HortError::ShellNotInRootfs { shell, path } => write!(
                f,
                "shell '{shell}' not found in rootfs '{path}' — set \"shell\" to one the rootfs provides, or omit it"
            ),
            HortError::WorkdirNotWritable { path } => write!(
                f,
                "rootfs '{path}': /workdir is not writable by the mapped uid — make it world-writable"
            ),
            HortError::DuplicateName { name } => write!(
                f,
                "a sandbox named '{name}' already exists (run 'hort attach {name}' to join it, or 'hort down {name}' first)"
            ),
            HortError::BranchExists { name } => write!(
                f,
                "branch '{name}' already exists; choose another name or use --branch to target an existing branch"
            ),
            HortError::BranchCheckedOut { branch } => {
                write!(f, "branch '{branch}' is already checked out in another worktree")
            }
            HortError::BranchDoesNotExist { branch, name } => write!(
                f,
                "branch '{branch}' does not exist; create it first or omit --branch to create a new branch named '{name}'"
            ),
            HortError::UpInProgress { name } => {
                write!(f, "another 'hort up {name}' is already in progress")
            }
            HortError::BranchRequiresGit => {
                write!(f, "--branch requires a git repository, but this project is not one")
            }
            HortError::SandboxNotRunning { name } => write!(
                f,
                "sandbox '{name}' is not running (run 'hort up {name}' to start it, or 'hort prune' to clean up the stale record)"
            ),
            HortError::UnknownSandboxOnAttach { name } => {
                write!(f, "no sandbox named '{name}' (run 'hort ls' to see what's alive)")
            }
            HortError::UnknownSandboxOnDown { name } => {
                write!(f, "no sandbox named '{name}' (run 'hort ls' to see what exists)")
            }
            HortError::RefusedWithoutConfirmation { command } => write!(
                f,
                "refusing to {command} without confirmation: stdin is not a TTY (pass --force to proceed)"
            ),
            HortError::InvalidConfig { detail } => write!(f, "invalid config: {detail}"),
            HortError::InvalidTimestamp { detail } => write!(f, "invalid timestamp: {detail}"),
            HortError::CorruptMetadata { detail } => write!(f, "corrupt metadata: {detail}"),
            HortError::GitCommandFailed { detail } => write!(f, "git command failed: {detail}"),
            HortError::ContainerRuntimeFailed { detail } => {
                write!(f, "container runtime failed: {detail}")
            }
            HortError::RuntimeUnavailable => {
                write!(f, "the container runtime is not available in this build")
            }
            HortError::StateIo { detail } => write!(f, "state directory error: {detail}"),
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

        assert_eq!(down.to_string(), "no sandbox named 'demo' (run 'hort ls' to see what exists)");
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

        assert_eq!(error.to_string(), "another 'hort up demo' is already in progress");
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
    fn user_namespaces_disabled_error_renders_canonical_string() {
        let error = HortError::UserNamespacesDisabled;

        assert_eq!(
            error.to_string(),
            "unprivileged user namespaces are disabled in this kernel — hort cannot create a sandbox"
        );
    }

    #[test]
    fn pasta_missing_error_renders_canonical_string() {
        let error = HortError::PastaMissing;

        assert_eq!(
            error.to_string(),
            "pasta not found on PATH — hort needs it for sandbox networking"
        );
    }

    #[test]
    fn no_rootfs_configured_error_renders_canonical_string() {
        let error = HortError::NoRootfsConfigured;

        assert_eq!(
            error.to_string(),
            "no rootfs configured — set \"rootfs\" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json"
        );
    }

    #[test]
    fn missing_rootfs_directory_error_renders_canonical_string() {
        let error = HortError::RootfsMissing { path: "/opt/hort/rootfs".to_string() };

        assert_eq!(
            error.to_string(),
            "rootfs directory '/opt/hort/rootfs' does not exist — prepare it first with podman export, debootstrap or umoci unpack"
        );
    }

    #[test]
    fn rootfs_without_shell_error_renders_canonical_string() {
        let error = HortError::RootfsWithoutShell { path: "/opt/hort/rootfs".to_string() };

        assert_eq!(
            error.to_string(),
            "rootfs '/opt/hort/rootfs' has no usable shell (expected /bin/sh) — the rootfs must provide one"
        );
    }

    #[test]
    fn configured_shell_missing_error_renders_canonical_string() {
        let error = HortError::ShellNotInRootfs {
            shell: "/bin/zsh".to_string(),
            path: "/opt/hort/rootfs".to_string(),
        };

        assert_eq!(
            error.to_string(),
            "shell '/bin/zsh' not found in rootfs '/opt/hort/rootfs' — set \"shell\" to one the rootfs provides, or omit it"
        );
    }

    #[test]
    fn workdir_not_writable_error_renders_canonical_string() {
        let error = HortError::WorkdirNotWritable { path: "/opt/hort/rootfs".to_string() };

        assert_eq!(
            error.to_string(),
            "rootfs '/opt/hort/rootfs': /workdir is not writable by the mapped uid — make it world-writable"
        );
    }

    #[test]
    fn error_exit_code_is_nonzero() {
        let error = HortError::DuplicateName { name: "demo".to_string() };

        assert_ne!(error.exit_code(), 0);
    }
}
