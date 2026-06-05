//! Teardown as a pure ordered plan (ADR-0006): `teardown_plan(&record) ->
//! Vec<TeardownStep>` in the mandatory order — host-side helpers → container →
//! worktree (omitted in no-git) → metadata. No execution here (CLAUDE.md c5).
//!
//! See backlog D-08.

// TODO(D-08): `TeardownStep` + the ordered plan.
