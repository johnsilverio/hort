//! `ls`: list every sandbox with its reconciled state and the figures a caller
//! needs to judge a forgotten box: session count, age, idle, branch, dirty
//! state, and how its git was built.
//!
//! It cross-checks the on-disk records against the live anchors and the
//! worktrees still on disk, joins each verdict back to its record, and derives
//! age from the recorded timestamps. Idle counts from the newest of those and
//! the last completion the sandbox announced, so a box whose agent has been
//! working all afternoon is not reported as untouched since its shell was
//! opened. Liveness comes from matching the record tokens against the registry
//! entries, so there is no liveness probe here. The dirty column is asked at
//! each record's own worktree path on disk, of the project that record names,
//! so a sandbox of another project reports its dirty state like any other; the forgotten box holding
//! uncommitted work is the one this listing exists to surface, and it is rarely
//! the box of the project you are standing in. The git mode is read the same
//! way, and a clone's commits are looked for in the same project. A record with a corrupt timestamp degrades only its own
//! row to an unknown age and idle, and the listing never mutates anything.

use std::time::{Duration, SystemTime};

use crate::commands::{last_announced_completion, present_worktrees};
use crate::domain::config::GitMode;
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
/// corrupt; `branch` is also `None` for a no-git record. `sessions` is `None` when
/// hort could not read the box's process list while its anchor was up, and `idle`
/// goes with it, having been computed from that same read: a row reporting three
/// hours untouched about a box hort never managed to look into reads as an
/// invitation to prune it. `dirty` is probed only
/// for a git record whose worktree is still on disk; it is `None` for no record,
/// a no-git record, an absent worktree, or a failed probe, all of which `ls`
/// reports as unknown rather than guessing. It stays an `Option<bool>` while
/// `prune` reads the same probe into three states, because here the answer is
/// displayed and there it decides a deletion.
pub struct LsEntry {
    pub name: SandboxName,
    pub state: SandboxState,
    pub sessions: Option<usize>,
    pub age: Option<Duration>,
    pub idle: Option<IdleState>,
    pub branch: Option<BranchName>,
    pub dirty: Option<bool>,
    pub git: Option<WorkdirGit>,
}

/// How a sandbox's `/workdir` carries its git, as `ls` reports it. `None` on the
/// entry means there is nothing to say: no record, a no-git record, or a
/// `/workdir` no longer on disk.
///
/// Only a clone is asked about its commits, because a worktree commits into the
/// project repository's own object store and cannot hold one the project lacks.
/// For a clone `unreturned` is `None` when hort could not tell, which a reader
/// must be able to see: a box read as holding nothing is one somebody collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkdirGit {
    Worktree,
    Clone { unreturned: Option<bool> },
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
                let sessions = self.observe_sessions(&name, state);
                let record = records.iter().find(|record| record.name() == &name);
                let dirty = record.and_then(|record| self.observe_dirty(record));
                let git = record.and_then(|record| self.observe_git(record));
                let last_event =
                    record.and_then(|record| last_announced_completion(self.notify, record));
                build_entry(name, state, sessions, record, Observed { dirty, git, last_event }, now)
            })
            .collect();

        Ok(entries)
    }

    /// How many sessions are joined to a sandbox, or unknown when hort could not
    /// look. A read that failed says nothing about who is inside, and this is the
    /// column a person reads to find that out, so it is reported as unknown rather
    /// than as a confident zero. The one exception is a sandbox with no live
    /// anchor: the anchor is pid 1 of the box's pid namespace and takes the
    /// namespace down with it, so there is nowhere left for a session to be and
    /// zero is an observation. Written as that exception and never as a list of
    /// the verdicts that mean the anchor is up, because three of the four do and a
    /// list that forgets one prints a confident zero over a box somebody is
    /// working in. A failure never fails the listing: one racing sandbox must not
    /// blind the rest.
    fn observe_sessions(&self, name: &SandboxName, state: SandboxState) -> Option<usize> {
        match self.sessions.session_pids(name) {
            Ok(pids) => Some(pids.len()),
            Err(_) if state == SandboxState::Orphaned => Some(0),
            Err(_) => None,
        }
    }

    /// Whether a sandbox's worktree is dirty, observed only when there is a git
    /// record whose worktree is still on disk at the path that record names. It
    /// is asked of the project the record names, since this listing is global
    /// and the repository it runs from keeps nothing for another project's
    /// worktree; a record naming no project leaves it unknown. A failed probe degrades to unknown, which `ls` reports honestly rather than
    /// guessing; nothing here gates a deletion, so unknown costs a dash.
    fn observe_dirty(&self, record: &SandboxRecord) -> Option<bool> {
        record.branch()?;
        let project = record.project_path()?;
        if !self.worktrees.exists(record.worktree_path()) {
            return None;
        }
        self.worktrees.is_dirty(record.name(), project).ok()
    }

    /// How a git record's `/workdir` was built, read from the disk at the
    /// record's own path, and for a clone whether it holds commits its own
    /// project lacks. The mode is asked only of a git record, because a no-git
    /// box mounts the user's folder and a `git init` there later would read as a
    /// clone hort never made. The commits are asked of the project the record
    /// names: this listing is global, and the repository it runs from would flag
    /// every other project's box. A record naming no project and a read that
    /// failed both leave the answer unknown, never "nothing held".
    fn observe_git(&self, record: &SandboxRecord) -> Option<WorkdirGit> {
        record.branch()?;
        match self.worktrees.git_mode_at(record.worktree_path())? {
            GitMode::Worktree => Some(WorkdirGit::Worktree),
            GitMode::Clone => {
                let unreturned = record.project_path().and_then(|project| {
                    self.worktrees.holds_unreturned_work(record.name(), project).ok()
                });
                Some(WorkdirGit::Clone { unreturned })
            }
        }
    }
}

/// What `ls` read about a record beyond its reconciled state and sessions.
struct Observed {
    dirty: Option<bool>,
    git: Option<WorkdirGit>,
    last_event: Option<SystemTime>,
}

fn build_entry(
    name: SandboxName,
    state: SandboxState,
    sessions: Option<usize>,
    record: Option<&SandboxRecord>,
    observed: Observed,
    now: SystemTime,
) -> LsEntry {
    let Observed { dirty, git, last_event } = observed;
    let Some(record) = record else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch: None, dirty, git };
    };

    let branch = record.branch().cloned();
    let parsed = (parse_timestamp(record.created_at()), parse_timestamp(record.last_attach_at()));
    let (Ok(created), Ok(attach)) = parsed else {
        return LsEntry { name, state, sessions, age: None, idle: None, branch, dirty, git };
    };

    LsEntry {
        name,
        state,
        sessions,
        age: Some(age(created, now)),
        idle: sessions.map(|count| idle(count, created, attach, last_event, now)),
        branch,
        dirty,
        git,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::fakes::{
        FakeNotifyProvider, FakeRegistry, FakeSessionProbe, FakeWorktreeProvider,
        InMemoryMetadataStore, ScriptedClock, sample_record,
    };

    /// A record written before hort remembered which project a sandbox was built
    /// from, the one kind of record that cannot name it.
    const RECORD_WITHOUT_PROJECT: &str = r#"{
        "schemaVersion": 1,
        "name": "demo",
        "branch": "demo",
        "worktreePath": "/state/sandboxes/demo/worktree-demo",
        "overlayPath": "/state/sandboxes/demo/overlay",
        "createdAt": "2026-06-11T12:00:00Z",
        "lastAttachAt": "2026-06-11T12:00:00Z",
        "notifyChannel": null,
        "token": null
    }"#;

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

        assert_eq!(entries[0].sessions, Some(3));
    }

    #[test]
    fn ls_reports_unknown_sessions_when_the_probe_fails_for_a_live_sandbox() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::failing();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // The column that says whether somebody is inside is the one column
        // where a confident zero costs the most, and the anchor of this box is
        // up: hort failed to look, which is not the same finding as looking and
        // finding nobody.
        assert_eq!(entries[0].sessions, None);
    }

    #[test]
    fn ls_degrades_idle_to_unknown_when_the_probe_fails_for_a_live_sandbox() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::failing();
        let now = humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Idle is computed from the same read: with no session count there is no
        // way to tell an hour untouched from an hour of somebody typing, and the
        // hour is the figure a person reads before deciding to prune by idle.
        assert_eq!(entries[0].idle, None);
    }

    #[test]
    fn ls_reports_unknown_sessions_when_the_probe_fails_for_a_lost_record() {
        let store = InMemoryMetadataStore::new();
        let registry =
            FakeRegistry::new(vec![(SandboxName::new("ghost").unwrap(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::failing();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // The question is whether the anchor is up, not whether the row is
        // `live`: three of the four verdicts say it is, and this one is the row
        // where the count decides something, since a lost record is the one hort
        // offers to adopt or clean. Cleaning it on a zero hort never observed
        // kills the box somebody is working in.
        assert_eq!(entries[0].sessions, None);
    }

    #[test]
    fn ls_reports_unknown_sessions_when_the_probe_fails_for_an_inconsistent_sandbox() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::failing();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // A worktree gone from under a live anchor is the third verdict that
        // means somebody can still be inside, and it is the one prune takes with
        // no idle threshold asked for at all. This row is the last place a person
        // sees who is in there before running it, so a zero hort never observed
        // spends that look.
        assert_eq!(entries[0].sessions, None);
    }

    #[test]
    fn ls_reports_no_sessions_when_the_probe_fails_for_an_orphaned_sandbox() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::failing();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Same failed read as the live box above, opposite answer, and the
        // verdict is what decides which: with no live anchor there is no pid
        // namespace left for a session to be in, so zero is an observation. A
        // reboot fails every one of these reads at once, and the listing that
        // says "orphaned, idle 3d" is the one that tells the user it can go.
        assert_eq!(entries[0].sessions, Some(0));
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
    fn ls_asks_a_worktree_whether_it_is_dirty_of_the_project_its_record_names() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_present_worktree(&name)
            .with_dirty_worktree(&name)
            .with_worktree_registered_only_in(&name, Path::new("/home/tester/projects/demo"));
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Only the project a worktree came from keeps the administrative
        // directory its state is read through. Asked of the repository the
        // listing runs from, every other project's box prints a dash, and the
        // uncommitted work in it goes unnoticed.
        assert_eq!(entries[0].dirty, Some(true));
    }

    #[test]
    fn ls_reports_dirty_as_unknown_for_a_record_that_names_no_project() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&serde_json::from_str::<SandboxRecord>(RECORD_WITHOUT_PROJECT).unwrap()).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_present_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // With no project to ask, the only repository left is the one the
        // listing runs from, whose answer is about some other project.
        assert_eq!(entries[0].dirty, None);
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

    #[test]
    fn ls_reports_a_sandbox_built_as_a_clone_in_clone_mode() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_clone_workdir(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].git, Some(WorkdirGit::Clone { unreturned: Some(false) }));
    }

    #[test]
    fn ls_reports_a_sandbox_built_as_a_worktree_in_worktree_mode() {
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

        assert_eq!(entries[0].git, Some(WorkdirGit::Worktree));
    }

    #[test]
    fn ls_reports_an_orphaned_clone_holding_commits_its_project_lacks() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        // No live anchor: the box after a reboot, which is when somebody reads
        // this listing to decide what is safe to collect.
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_clone_workdir(&name).with_unreturned_work(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        assert_eq!(entries[0].state, SandboxState::Orphaned);
        assert_eq!(entries[0].git, Some(WorkdirGit::Clone { unreturned: Some(true) }));
    }

    #[test]
    fn ls_asks_a_clone_about_its_commits_of_the_project_its_record_names() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_work_returned_only_to(&name, Path::new("/home/tester/projects/demo"));
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // `ls` is global. Asked of any repository but the one the box came from,
        // this clone reads as holding work, and every box of every other project
        // would be flagged from wherever the listing happened to run.
        assert_eq!(entries[0].git, Some(WorkdirGit::Clone { unreturned: Some(false) }));
    }

    #[test]
    fn ls_reports_unreturned_work_as_unknown_when_the_read_fails() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_failing_unreturned_work_probe(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Printed as "nothing held", an unread clone is one somebody collects.
        assert_eq!(entries[0].git, Some(WorkdirGit::Clone { unreturned: None }));
    }

    #[test]
    fn ls_reports_unreturned_work_as_unknown_for_a_record_that_names_no_project() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&serde_json::from_str::<SandboxRecord>(RECORD_WITHOUT_PROJECT).unwrap()).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_clone_workdir(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // With no project to ask, there is no repository whose answer means
        // anything, and the one the listing runs from is somebody else's.
        assert_eq!(entries[0].git, Some(WorkdirGit::Clone { unreturned: None }));
    }

    #[test]
    fn ls_reports_no_git_mode_for_a_no_git_record_whose_folder_became_a_repository() {
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
        let worktrees = FakeWorktreeProvider::new().with_clone_workdir(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let notify = FakeNotifyProvider::new();
        let command = ls_command(&store, &registry, &worktrees, &sessions, &clock, &notify);

        let entries = command.run().unwrap();

        // Without git the box mounts the user's own folder, and a `git init` run
        // there later gives it a `.git` directory that reads exactly like a
        // clone's. The mode is what hort built, so a box it built with no git
        // has none to report.
        assert_eq!(entries[0].git, None);
    }
}
