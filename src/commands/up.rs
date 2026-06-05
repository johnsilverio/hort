//! `up <name>`: walking-skeleton slice 1. Order (PRD §3.1): per-name flock →
//! merge config → (git) branch + worktree → persist metadata BEFORE the
//! container → build container → record token → watcher if notify → first
//! session unless `-d`. Reentrant against half-built state.
//!
//! See backlog C-01.

// TODO(C-01): the UpCommand coordinator.
