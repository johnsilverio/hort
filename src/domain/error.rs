//! `HortError`: an enum whose specific variants each carry their VERBATIM
//! canonical message (PRD §3.0; catalogue in backlog §14). No catch-all
//! `Other(String)`. `InvalidName` is a unit variant. Hand-written `Display`/
//! `Error` + `exit_code()` (no `thiserror` — it is not in the dependency survey).
//!
//! See backlog D-02.

// TODO(D-02): the error enum + verbatim canonical messages + exit_code().
