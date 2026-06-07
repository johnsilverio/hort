//! `HortError`: the domain error type. Specific variants carry their verbatim
//! canonical message inline (no catch-all `Other(String)` that would drop the
//! wording), with hand-written `Display`/`Error` and `exit_code()` (no
//! `thiserror`). Only `InvalidName` exists so far.

/// Domain error type.
///
/// Carries only the `InvalidName` unit variant: the signal a validated newtype
/// (`SandboxName`/`BranchName`/`Domain`) returns when it rejects its input.
#[derive(Debug)]
pub enum HortError {
    /// A validated domain newtype rejected its input. Matched as a **unit**
    /// variant (`HortError::InvalidName`), never `HortError::InvalidName { .. }`.
    InvalidName,
}

// TODO(D-02): the specific variants + verbatim canonical messages + Display/Error + exit_code().
