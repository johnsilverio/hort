//! `ls`: list every sandbox with its reconciled state and the figures a caller
//! needs to judge a forgotten box: session count, age, idle, and branch.
//!
//! It cross-checks the on-disk records against the live anchors and the
//! worktrees still on disk, joins each verdict back to its record, and derives
//! age from the recorded timestamps. Idle counts from the newest of those and
//! the last completion the sandbox announced, so a box whose agent has been
//! working all afternoon is not reported as untouched since its shell was
//! opened. Liveness comes from matching the record tokens against the registry
//! entries, so there is no liveness probe here. The dirty column is asked at
//! each record's own worktree path on disk, so a sandbox of another project
//! reports its dirty state like any other; the forgotten box holding
//! uncommitted work is the one this listing exists to surface, and it is rarely
//! the box of the project you are standing in. A record with a corrupt
//! timestamp degrades only its own row to an unknown age and idle, and the
//! listing never mutates anything.

use std::time::{Duration, SystemTime};

use crate::commands::{last_announced_completion, present_worktrees};
use crate::domain::error::HortError;
use crate::domain::idle::{IdleState, age, idle, parse_timestamp};
use crate::domain::model::{BranchName, SandboxName, SandboxRecord};
use crate::domain::reconcile::{SandboxState, reconcile_all};
use crate::ports::{
    Clock, ContainerRegistry, MetadataStore, NotifyProvider, SessionProbe, WorktreeProvider,
};

/// One row of `ls` output: a sandbox's reconciled state plus the figures the CLI
/// renders beside it. `age`, `idle`, and `branch` are `None` when there is no
/// record to derive them from (a lost-record row) or the record's timestamps are
/// corrupt; `branch` is also `None` for a no-git record. `dirty` is probed only
/// for a git record whose worktree is still on disk; it is `None` for no record,
/// a no-git record, an absent worktree, or a failed probe, all of which `ls`
/// reports as unknown rather than guessing. It stays an `Option<bool>` while
/// `prune` reads the same probe into three states, because here the answer is
/// displayed and there it decides a deletion.
pub struct LsEntry {
    pub name: SandboxName,
    pub state: SandboxState,
    pub sessions: usize,
    pub age: Option<Duration>,
    pub idle: Option<IdleState>,
    pub branch: Option<BranchName>,
    pub dirty: Option<bool>,
}

/// Coordinates `ls` over the read ports it depends on. It carries no
/// `LivenessProbe`: liveness is derived by matching the record tokens against the
/// registry entries, the cross-source reconciliation contract.
pub struct LsCommand<'a> {
    store: &'a dyn MetadataStore,
    registry: &'a dyn ContainerRegistry,
    worktrees: &'a dyn WorktreeProvider,
    sessions: &'a dyn SessionProbe,
    clock: &'a dyn Clock,
    notify: &'a dyn NotifyProvider,
}

impl<'a> LsCommand<'a> {
    pub fn new(
        store: &'a dyn MetadataStore,
        registry: &'a dyn ContainerRegistry,
        worktrees: &'a dyn WorktreeProvider,
        sessions: &'a dyn SessionProbe,
        clock: &'a dyn Clock,
        notify: &'a dyn NotifyProvider,
    ) -> Self {
        Self { store, registry, worktrees, sessions, clock, notify }
    }
}

impl LsCommand<'_> {
    pub fn run(&self) -> Result<Vec<LsEntry>, HortError> {
        let records = self.store.list()?;
        let live = self.registry.list_live()?;
        let present = present_worktrees(self.worktrees, &records);
        let now = self.clock.now();

        let verdicts = reconcile_all(&records, &live, &present);

        let entries: Vec<LsEntry> = verdicts
            .into_iter()
            .map(|(name, state)| {
                // A probe error reads as zero sessions rather than failing the
                // whole listing: a single racing sandbox must not blind the rest.
                let sessions = self.sessions.session_pids(&name).map_or(0, |pids| pids.len());
                let record = records.iter().find(|record| record.name() == &name);
                let dirty = record.and_then(|record| self.observe_dirty(record));
                let last_event =
                    record.and_then(|record| last_announced_completion(self.notify, record));
                build_entry(name, state, sessions, record, dirty, last_event, now)
            })
            .collect();

        Ok(entries)
    }

    /// Whether a sandbox's worktree is dirty, observed only when there is a git
    /// record whose worktree is still on disk at the path that record names. A
    /// failed probe degrades to unknown, which `ls` reports honestly rather than
    /// guessing; nothing here gates a deletion, so unknown costs a dash.
    fn observe_dirty(&self, record: &SandboxRecord) -> Option<bool> {
        record.branch()?;
        if !self.worktrees.exists(record.worktree_path()) {
            return None;
        }
        self.worktrees.is_dirty(record.name()).ok()
    }
}

fn build_entry(
    name: SandboxName,
    state: SandboxState,
    sessions: usize,
    record: Option<&SandboxRecord>,
    dirty: Option<bool>,
    last_event: Option<SystemTime>,
    now: SystemTime,
) -> LsEntry {
    let Some(record) = record else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch: None, dirty };
    };

    let branch = record.branch().cloned();
    let parsed = (parse_timestamp(record.created_at()), parse_timestamp(record.last_attach_at()));
    let (Ok(created), Ok(attach)) = parsed else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch, dirty };
    };

    LsEntry {
        name,
        state,
        sessions,
        age: Some(age(created, now)),
        idle: Some(idle(sessions, created, attach, last_event, now)),
        branch,
        dirty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::SystemTime;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::fakes::{
        FakeNotifyProvider, FakeRegistry, FakeSessionProbe, FakeWorktreeProvider,
        InMemoryMetadataStore, ScriptedClock, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn ls_command<'a>(
        store: &'a InMemoryMetadataStore,
        registry: &'a FakeRegistry,
        worktrees: &'a FakeWorktreeProvider,
        sessions: &'a FakeSessionProbe,
        clock: &'a ScriptedClock,
        notify: &'a FakeNotifyProvider,
    ) -> LsCommand<'a> {
        LsCommand { store, registry, worktrees, sessions, clock, notify }
    }

    #[test]
    fn ls_reports_live_for_running_anchor() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, name);
        assert_eq!(entries[0].state, SandboxState::Live);
    }

    #[test]
    fn ls_reports_orphaned_when_anchor_pid_is_gone() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Orphaned);
    }

    #[test]
    fn ls_reports_lost_record_for_live_anchor_without_metadata() {
        let store = InMemoryMetadataStore::new();
        let registry =
            FakeRegistry::new(vec![(SandboxName::new("ghost").unwrap(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, SandboxName::new("ghost").unwrap());
        assert_eq!(entry.state, SandboxState::LostRecord);
        assert_eq!(entry.age, None);
        assert_eq!(entry.idle, None);
        assert_eq!(entry.branch, None);
    }

    #[test]
    fn ls_reports_inconsistent_when_worktree_gone() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Inconsistent);
    }

    #[test]
    fn ls_reports_live_for_a_worktree_this_repository_does_not_list() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_present_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // A sandbox's worktree lives under a path hort owns, not under the
        // project the user happens to be standing in, so what answers "is it
        // still there" is the disk. Asking the current repository's list instead
        // makes every sandbox of every other project read as one whose worktree
        // vanished, from anywhere but its own directory.
        assert_eq!(entries[0].state, SandboxState::Live);
    }

    #[test]
    fn ls_never_mutates_state() {
        let name = SandboxName::new("demo").unwrap();
        let record = sample_record("demo").with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        command.run().unwrap();

        assert_eq!(store.list().unwrap(), vec![record]);
    }

    #[test]
    fn ls_counts_sessions_from_probe() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![111, 222, 333]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].sessions, 3);
    }

    #[test]
    fn ls_reports_age_from_created_at() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let now = humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].age, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn ls_reports_idle_from_last_attach_when_no_sessions() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            Some(BranchName::new("demo").unwrap()),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-11T12:00:00Z".to_string(),
            "2026-06-11T12:30:00Z".to_string(),
            None,
            PathBuf::from("/home/tester/projects/demo"),
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let now = humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].idle, Some(IdleState::Idle(Duration::from_secs(1800))));
    }

    #[test]
    fn ls_counts_idle_from_the_last_completion_event() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            Some(BranchName::new("demo").unwrap()),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-11T09:00:00Z".to_string(),
            "2026-06-11T10:00:00Z".to_string(),
            Some("desktop".to_string()),
            PathBuf::from("/home/tester/projects/demo"),
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let now = humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let finished = humantime::parse_rfc3339("2026-06-11T12:50:00Z").unwrap();
        let notify = FakeNotifyProvider::new().with_last_event_at(finished);
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Three hours since the shell was closed, ten minutes since the agent
        // announced it was done. This is the box the listing exists to keep from
        // being lost, and counting from the attach reports it as untouched all
        // afternoon while it was working the whole time.
        assert_eq!(entries[0].idle, Some(IdleState::Idle(Duration::from_secs(600))));
    }

    #[test]
    fn ls_does_not_ask_for_a_completion_time_without_a_declared_channel() {
        let announcing = SandboxName::new("announcing").unwrap();
        let silent = SandboxName::new("silent").unwrap();
        let store = InMemoryMetadataStore::new();
        store
            .put(
                &SandboxRecord::new(
                    announcing.clone(),
                    Some(BranchName::new("announcing").unwrap()),
                    PathBuf::from("/state/sandboxes/announcing/worktree-announcing"),
                    PathBuf::from("/state/sandboxes/announcing/overlay"),
                    "2026-06-11T12:00:00Z".to_string(),
                    "2026-06-11T12:00:00Z".to_string(),
                    Some("desktop".to_string()),
                    PathBuf::from("/home/tester/projects/announcing"),
                )
                .with_token(canned_token()),
            )
            .unwrap();
        store.put(&sample_record("silent").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![
            (announcing.clone(), canned_token()),
            (silent.clone(), canned_token()),
        ]);
        let worktrees = FakeWorktreeProvider::new()
            .with_listed_worktree(&announcing)
            .with_listed_worktree(&silent);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        command.run().unwrap();

        // Both halves, and the first is what makes the second mean anything: a
        // listing that asked nobody would satisfy the denial while pinning
        // nothing. What the record says is the memory of what the build actually
        // made, so a box whose channel was never created is never stat'd for a
        // file that cannot be there, whatever the configuration claims today.
        assert_eq!(notify.asked_for_last_event(), vec![announcing]);
    }

    #[test]
    fn ls_reports_active_idle_while_sessions_run() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![111]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].idle, Some(IdleState::Active));
    }

    #[test]
    fn ls_degrades_age_to_unknown_on_corrupt_timestamp() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            Some(BranchName::new("demo").unwrap()),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "not a timestamp".to_string(),
            "not a timestamp".to_string(),
            None,
            PathBuf::from("/home/tester/projects/demo"),
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Live);
        assert_eq!(entries[0].age, None);
        assert_eq!(entries[0].idle, None);
    }

    #[test]
    fn ls_reports_dirty_worktree() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].dirty, Some(true));
    }

    #[test]
    fn ls_reports_dirty_for_a_worktree_this_repository_does_not_list() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees =
            FakeWorktreeProvider::new().with_present_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // A box of another project is the most forgettable one there is, and it
        // is the box holding uncommitted work that `ls` exists to keep from
        // being lost. Printing a dash for it from anywhere but its own directory
        // hides exactly the row that needed attention.
        assert_eq!(entries[0].dirty, Some(true));
    }

    #[test]
    fn ls_degrades_dirty_to_unknown_when_probe_fails() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_failing_dirty_probe(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].dirty, None);
    }

    #[test]
    fn ls_reports_no_dirty_for_no_git_record() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            None,
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-11T12:00:00Z".to_string(),
            "2026-06-11T12:00:00Z".to_string(),
            None,
            PathBuf::from("/home/tester/projects/demo"),
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].dirty, None);
    }

    #[test]
    fn ls_reports_no_dirty_for_vanished_worktree() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].dirty, None);
    }
}
