//! Two-layer reconciliation (ADR-0005), observable reality wins:
//! - per-record `SandboxRecord::reconcile(&probe) -> SandboxState` → `Live`/`Orphaned` only;
//! - cross-source `Reconciler` (records ↔ container-state registry ↔ `git worktree list`)
//!   adds `LostRecord` and `Inconsistent`.
//!
//! See backlog D-06, D-07.

// TODO(D-06/D-07): `SandboxState`, the per-record verdict, and the pure
//                  cross-source reconciler over plain lists.
