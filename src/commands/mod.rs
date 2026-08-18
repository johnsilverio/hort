//! Commands: the coordinators that wire ports + domain into use-cases
//! (`up`/`attach`/`ls`/`down`/`prune`/`config`/`doctor`). They depend on the
//! domain plus trait ports, never on a concrete adapter (architecture.md).

use std::time::SystemTime;

use crate::domain::model::SandboxRecord;
use crate::ports::{NotifyProvider, Worktree, WorktreeProvider};

pub mod attach;
pub mod config;
pub mod doctor;
pub mod down;
pub mod ls;
pub mod prune;
pub mod up;

/// The worktrees reconciliation counts as present: the recorded path of every
/// sandbox whose worktree is still on disk.
///
/// Presence is answered by the disk rather than by the current repository's
/// worktree list, because a sandbox's worktree lives under a path hort owns and
/// no repository but its own ever lists it. Asking the list would read every
/// sandbox of every other project as one whose worktree vanished, which is a
/// state both `ls` and `prune` treat as debris.
fn present_worktrees(worktrees: &dyn WorktreeProvider, records: &[SandboxRecord]) -> Vec<Worktree> {
    records
        .iter()
        .map(SandboxRecord::worktree_path)
        .filter(|path| worktrees.exists(path))
        .map(|path| Worktree { path: path.to_path_buf() })
        .collect()
}

/// When a sandbox's agent last announced that it finished, or `None` if it never
/// has and for every sandbox that cannot announce anything at all.
///
/// The channel is asked for only when the record names one, and the record is
/// asked rather than the configuration: the record is the memory of what the
/// build actually made, while the configuration can gain or lose a channel while
/// the box is standing. `ls` and `prune` both fold this into the same idle
/// arithmetic, so it lives here rather than in either of them, where one could be
/// fixed and the other left answering the same question differently.
fn last_announced_completion(
    notify: &dyn NotifyProvider,
    record: &SandboxRecord,
) -> Option<SystemTime> {
    record.notify_channel()?;
    notify.last_event_at(record.name())
}
