//! `ls`: list every sandbox with its reconciled state and the figures a caller
//! needs to judge a forgotten box: session count, age, idle, and branch.
//!
//! It cross-checks the on-disk records against the live anchors and the worktree
//! list, joins each verdict back to its record, and derives age and idle from the
//! recorded timestamps. Liveness comes from matching the record tokens against
//! the registry entries, so there is no liveness probe here. A record with a
//! corrupt timestamp degrades only its own row to an unknown age and idle, and
//! the listing never mutates anything.

use std::time::{Duration, SystemTime};

use crate::domain::error::HortError;
use crate::domain::idle::{IdleState, age, idle, parse_timestamp};
use crate::domain::model::{BranchName, SandboxName, SandboxRecord};
use crate::domain::reconcile::{SandboxState, reconcile_all};
use crate::ports::{Clock, ContainerRegistry, MetadataStore, SessionProbe, WorktreeProvider};

/// One row of `ls` output: a sandbox's reconciled state plus the figures the CLI
/// renders beside it. `age`, `idle`, and `branch` are `None` when there is no
/// record to derive them from (a lost-record row) or the record's timestamps are
/// corrupt; `branch` is also `None` for a no-git record.
pub struct LsEntry {
    pub name: SandboxName,
    pub state: SandboxState,
    pub sessions: usize,
    pub age: Option<Duration>,
    pub idle: Option<IdleState>,
    pub branch: Option<BranchName>,
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
}

impl<'a> LsCommand<'a> {
    pub fn new(
        store: &'a dyn MetadataStore,
        registry: &'a dyn ContainerRegistry,
        worktrees: &'a dyn WorktreeProvider,
        sessions: &'a dyn SessionProbe,
        clock: &'a dyn Clock,
    ) -> Self {
        Self { store, registry, worktrees, sessions, clock }
    }
}

impl LsCommand<'_> {
    pub fn run(&self) -> Result<Vec<LsEntry>, HortError> {
        let records = self.store.list()?;
        let live = self.registry.list_live()?;
        let listed = self.worktrees.list()?;
        let now = self.clock.now();

        let verdicts = reconcile_all(&records, &live, &listed);

        let entries: Vec<LsEntry> = verdicts
            .into_iter()
            .map(|(name, state)| {
                // A probe error reads as zero sessions rather than failing the
                // whole listing: a single racing sandbox must not blind the rest.
                let sessions = self.sessions.session_pids(&name).map_or(0, |pids| pids.len());
                let record = records.iter().find(|record| record.name() == &name);
                build_entry(name, state, sessions, record, now)
            })
            .collect();

        Ok(entries)
    }
}

fn build_entry(
    name: SandboxName,
    state: SandboxState,
    sessions: usize,
    record: Option<&SandboxRecord>,
    now: SystemTime,
) -> LsEntry {
    let Some(record) = record else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch: None };
    };

    let branch = record.branch().cloned();
    let parsed = (parse_timestamp(record.created_at()), parse_timestamp(record.last_attach_at()));
    let (Ok(created), Ok(attach)) = parsed else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch };
    };

    LsEntry {
        name,
        state,
        sessions,
        age: Some(age(created, now)),
        idle: Some(idle(sessions, created, attach, None, now)),
        branch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::SystemTime;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::fakes::{
        FakeRegistry, FakeSessionProbe, FakeWorktreeProvider, InMemoryMetadataStore, ScriptedClock,
        sample_record,
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
    ) -> LsCommand<'a> {
        LsCommand { store, registry, worktrees, sessions, clock }
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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Inconsistent);
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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let now = humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].idle, Some(IdleState::Idle(Duration::from_secs(1800))));
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
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

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
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock);

        let entries = command.run().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, SandboxState::Live);
        assert_eq!(entries[0].age, None);
        assert_eq!(entries[0].idle, None);
    }
}
