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
#[derive(Debug)]
pub enum HortError {
    /// A validated domain newtype rejected its input. Matched as a **unit**
    /// variant (`HortError::InvalidName`), never `HortError::InvalidName { .. }`.
    InvalidName,
    /// `up`: a fully built sandbox of this name already exists.
    DuplicateName { name: String },
    /// `attach`: the name has metadata but no live anchor.
    SandboxNotRunning { name: String },
    /// `attach`: no sandbox of this name is known ("what's alive" wording).
    UnknownSandboxOnAttach { name: String },
    /// `down`: no sandbox of this name is known ("what exists" wording).
    UnknownSandboxOnDown { name: String },
    /// Config parsing failed: the input was not valid JSONC. Carries a
    /// human-readable detail; the rendered message is not a canonical product
    /// string, so callers match the variant, not the text.
    InvalidConfig { detail: String },
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
            HortError::InvalidConfig { detail } => write!(f, "invalid config: {detail}"),
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
    fn error_exit_code_is_nonzero() {
        let error = HortError::DuplicateName { name: "demo".to_string() };

        assert_ne!(error.exit_code(), 0);
    }
}
