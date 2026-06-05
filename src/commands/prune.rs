//! `prune`: explicit, confirmed cleanup of idle sandboxes + abrupt-death
//! orphans; excludes dirty sandboxes and live-project caches unless `--force`;
//! refuses without a TTY; mandatory teardown order (PRD §3.5).
//!
//! See backlog C-05.

// TODO(C-05): the PruneCommand coordinator.
