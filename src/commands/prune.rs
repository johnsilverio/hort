//! `prune`: explicit, confirmed cleanup of idle sandboxes and abrupt-death
//! debris (orphaned and inconsistent sandboxes, plus corrupt metadata dirs).
//!
//! It reconciles the records against the live anchors and worktrees, gathers the
//! corrupt dirs, observes idle and dirty state, and runs the pure selection. An
//! empty removal set never prompts; otherwise, without `--force`, a non-TTY stdin
//! refuses and a TTY prompts with every candidate name listed first. Each chosen
//! removal follows the mandatory teardown order, and a single stale-registration
//! sweep runs at the end of every non-refused, non-declined run.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::domain::error::HortError;
use crate::domain::idle::{IdleState, idle, parse_timestamp};
use crate::domain::model::{SandboxName, SandboxRecord};
use crate::domain::prune::{CorruptInput, PruneInput, PrunePlan, PruneSkip, prune_selection};
use crate::domain::reconcile::reconcile_all;
use crate::domain::teardown::{TeardownStep, teardown_plan};
use crate::ports::{
    Clock, Confirmer, ContainerRegistry, ContainerRuntime, MetadataStore, NetworkProvider,
    SessionProbe, Worktree, WorktreeProvider,
};

/// What a `prune` run removed and what it skipped, with the reason for each skip.
pub struct PruneReport {
    pub removed: Vec<String>,
    pub skipped: Vec<PruneSkip>,
}

/// Coordinates `prune` over the ports it depends on. It carries the state root so
/// it can derive the canonical worktree path of a corrupt entry, which has no
/// record to read the path from.
pub struct PruneCommand<'a> {
    store: &'a dyn MetadataStore,
    registry: &'a dyn ContainerRegistry,
    worktrees: &'a dyn WorktreeProvider,
    sessions: &'a dyn SessionProbe,
    clock: &'a dyn Clock,
    confirmer: &'a dyn Confirmer,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    state_root: PathBuf,
}

impl<'a> PruneCommand<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: &'a dyn MetadataStore,
        registry: &'a dyn ContainerRegistry,
        worktrees: &'a dyn WorktreeProvider,
        sessions: &'a dyn SessionProbe,
        clock: &'a dyn Clock,
        confirmer: &'a dyn Confirmer,
        runtime: &'a dyn ContainerRuntime,
        network: &'a dyn NetworkProvider,
        state_root: PathBuf,
    ) -> Self {
        Self { store, registry, worktrees, sessions, clock, confirmer, runtime, network, state_root }
    }
}

impl PruneCommand<'_> {
    pub fn run(
        &self,
        idle_threshold: Option<Duration>,
        force: bool,
        stdin_is_tty: bool,
    ) -> Result<PruneReport, HortError> {
        let records = self.store.list()?;
        let live = self.registry.list_live()?;
        let listed = self.worktrees.list()?;
        let corrupt = self.store.list_corrupt()?;
        let now = self.clock.now();

        let inputs: Vec<PruneInput> = reconcile_all(&records, &live, &listed)
            .into_iter()
            .filter_map(|(name, state)| {
                let record = records.iter().find(|record| record.name() == &name)?;
                let idle = self.sandbox_idle(record, now);
                let dirty = self.sandbox_dirty(record, &listed);
                Some(PruneInput { name, state, idle, dirty })
            })
            .collect();

        let corrupt_inputs: Vec<CorruptInput> = corrupt
            .iter()
            .map(|entry| CorruptInput {
                name: entry.name.clone(),
                dirty: self.corrupt_dirty(&entry.name, &listed),
            })
            .collect();

        let plan = prune_selection(&inputs, &corrupt_inputs, idle_threshold, force);

        if plan.sandboxes.is_empty() && plan.corrupt.is_empty() {
            self.worktrees.prune_stale()?;
            return Ok(PruneReport { removed: Vec::new(), skipped: plan.skipped });
        }

        if !force {
            if !stdin_is_tty {
                return Err(HortError::RefusedWithoutConfirmation { command: "prune".to_string() });
            }
            if !self.confirmer.confirm(&confirmation_message(&plan))? {
                return Ok(PruneReport { removed: Vec::new(), skipped: Vec::new() });
            }
        }

        let mut removed = Vec::new();
        for name in &plan.sandboxes {
            let Some(record) = records.iter().find(|record| record.name() == name) else {
                continue;
            };
            for step in teardown_plan(record) {
                match step {
                    // No watcher pid is persisted yet, so there is nothing to stop;
                    // the watcher-stop seam lands with the notify watcher work.
                    TeardownStep::StopWatcher => {}
                    TeardownStep::StopNetwork => self.network.teardown(name)?,
                    TeardownStep::StopContainer => self.runtime.teardown(name)?,
                    TeardownStep::RemoveWorktree => self.worktrees.remove(name)?,
                    TeardownStep::RemoveMetadata => self.store.remove(name)?,
                }
            }
            removed.push(name.as_str().to_string());
        }

        for name in &plan.corrupt {
            let sandbox_name = SandboxName::new(name)?;
            self.network.teardown(&sandbox_name)?;
            self.runtime.teardown(&sandbox_name)?;
            self.worktrees.remove(&sandbox_name)?;
            self.store.remove(&sandbox_name)?;
            removed.push(name.clone());
        }

        self.worktrees.prune_stale()?;
        Ok(PruneReport { removed, skipped: plan.skipped })
    }

    /// The idle state of a record, derived exactly as `ls` does it: a session
    /// probe failure reads as zero sessions, and an unreadable timestamp leaves
    /// idle unknown rather than failing the run.
    fn sandbox_idle(&self, record: &SandboxRecord, now: SystemTime) -> Option<IdleState> {
        let sessions = self.sessions.session_pids(record.name()).map_or(0, |pids| pids.len());
        let (Ok(created), Ok(attach)) =
            (parse_timestamp(record.created_at()), parse_timestamp(record.last_attach_at()))
        else {
            return None;
        };
        Some(idle(sessions, created, attach, None, now))
    }

    /// Whether a record's worktree is dirty, probed only when there is something
    /// to protect: a no-git record or an absent worktree has nothing to lose, so
    /// it is `None`. An unanswerable probe reads as dirty, because the gate
    /// guards uncommitted work and `--force` is the only override.
    fn sandbox_dirty(&self, record: &SandboxRecord, listed: &[Worktree]) -> Option<bool> {
        record.branch()?;
        if !path_listed(listed, record.worktree_path()) {
            return None;
        }
        Some(self.worktrees.is_dirty(record.name()).unwrap_or(true))
    }

    /// Whether a corrupt dir's worktree is dirty. A corrupt entry has no record
    /// to read the path from, so the canonical state-root layout supplies it; the
    /// same probe-error-as-dirty mapping applies once the worktree is present.
    fn corrupt_dirty(&self, name: &str, listed: &[Worktree]) -> Option<bool> {
        let path = self.corrupt_worktree_path(name);
        if !path_listed(listed, &path) {
            return None;
        }
        let sandbox_name = SandboxName::new(name).ok()?;
        Some(self.worktrees.is_dirty(&sandbox_name).unwrap_or(true))
    }

    fn corrupt_worktree_path(&self, name: &str) -> PathBuf {
        self.state_root.join("sandboxes").join(name).join(format!("worktree-{name}"))
    }
}

fn path_listed(listed: &[Worktree], path: &Path) -> bool {
    listed.iter().any(|worktree| worktree.path == *path)
}

/// A confirmation message naming every candidate, sandbox and corrupt dir alike,
/// so the user sees what would be removed. The wording is not a product
/// guarantee; only that the names are present.
fn confirmation_message(plan: &PrunePlan) -> String {
    let mut names: Vec<&str> = plan.sandboxes.iter().map(SandboxName::as_str).collect();
    names.extend(plan.corrupt.iter().map(String::as_str));
    format!("prune {}? this removes the worktree and metadata", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    use crate::domain::model::{AnchorPid, BranchName, LivenessToken, MountNsInode, SandboxName, SandboxRecord};
    use crate::domain::prune::SkipReason;
    use crate::fakes::{
        FakeConfirmer, FakeNetwork, FakeRegistry, FakeRuntime, FakeSessionProbe,
        FakeWorktreeProvider, InMemoryMetadataStore, ScriptedClock, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn state_root() -> PathBuf {
        PathBuf::from("/state")
    }

    #[allow(clippy::too_many_arguments)]
    fn prune_command<'a>(
        store: &'a InMemoryMetadataStore,
        registry: &'a FakeRegistry,
        worktrees: &'a FakeWorktreeProvider,
        sessions: &'a FakeSessionProbe,
        clock: &'a ScriptedClock,
        confirmer: &'a FakeConfirmer,
        runtime: &'a FakeRuntime,
        network: &'a FakeNetwork,
    ) -> PruneCommand<'a> {
        PruneCommand {
            store,
            registry,
            worktrees,
            sessions,
            clock,
            confirmer,
            runtime,
            network,
            state_root: state_root(),
        }
    }

    #[test]
    fn prune_removes_orphaned_sandbox_in_teardown_order() {
        let name = SandboxName::new("demo").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(None, true, false).unwrap();

        let expected = vec![
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
            "worktrees.remove".to_string(),
            "store.remove".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn prune_removes_corrupt_entry_in_teardown_order() {
        let rotten = SandboxName::new("rotten").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new()
            .with_corrupt_entry("rotten", "broken json")
            .with_trace(trace.clone());
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&rotten).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(None, true, false).unwrap();

        let expected = vec![
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
            "worktrees.remove".to_string(),
            "store.remove".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn prune_removes_corrupt_metadata_entry() {
        let store = InMemoryMetadataStore::new().with_corrupt_entry("rotten", "broken json");
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, true, false).unwrap();

        assert!(report.removed.contains(&"rotten".to_string()));
        assert!(store.list_corrupt().unwrap().is_empty());
    }

    #[test]
    fn prune_skips_corrupt_entry_with_dirty_worktree() {
        let rotten = SandboxName::new("rotten").unwrap();
        let store = InMemoryMetadataStore::new().with_corrupt_entry("rotten", "broken json");
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&rotten).with_dirty_worktree(&rotten);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, false, false).unwrap();

        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "rotten".to_string(), reason: SkipReason::Dirty }]
        );
        assert_eq!(store.list_corrupt().unwrap().len(), 1);
    }

    #[test]
    fn prune_lists_candidates_in_confirmation_prompt() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new().with_corrupt_entry("rotten", "broken json");
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(None, false, true).unwrap();

        let prompts = confirmer.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("demo"));
        assert!(prompts[0].contains("rotten"));
    }

    #[test]
    fn prune_refuses_without_tty_confirmation() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let result = command.run(None, false, false);

        assert_eq!(
            result.err(),
            Some(HortError::RefusedWithoutConfirmation { command: "prune".to_string() })
        );
        assert!(store.get(&name).unwrap().is_some());
    }

    #[test]
    fn prune_skips_confirmation_with_force() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(None, true, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn prune_aborts_when_confirmation_declined() {
        let name = SandboxName::new("demo").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, false, true).unwrap();

        assert!(report.removed.is_empty());
        assert!(report.skipped.is_empty());
        assert!(store.get(&name).unwrap().is_some());
        assert!(trace.borrow().is_empty());
    }

    #[test]
    fn prune_does_not_prompt_with_empty_plan() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, false, false).unwrap();

        assert!(report.removed.is_empty());
        assert!(report.skipped.is_empty());
        assert!(confirmer.prompts().is_empty());
    }

    #[test]
    fn prune_reports_skipped_dirty_sandbox() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, false, false).unwrap();

        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Dirty }]
        );
        assert!(store.get(&name).unwrap().is_some());
    }

    #[test]
    fn prune_treats_dirty_probe_failure_as_dirty() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_failing_dirty_probe(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        let report = command.run(None, false, false).unwrap();

        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Dirty }]
        );
    }

    #[test]
    fn prune_clears_stale_worktree_registrations() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(None, false, false).unwrap();

        assert_eq!(worktrees.prune_stale_calls(), 1);
    }

    #[test]
    fn prune_removes_idle_sandbox_beyond_threshold() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            Some(BranchName::new("demo").unwrap()),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-12T12:00:00Z".to_string(),
            "2026-06-12T12:00:00Z".to_string(),
            None,
        )
        .with_token(canned_token());
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_listed_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let now = humantime::parse_rfc3339("2026-06-12T13:00:00Z").unwrap();
        let clock = ScriptedClock::new(now);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let command =
            prune_command(&store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network);

        command.run(Some(Duration::from_secs(1800)), true, false).unwrap();

        assert_eq!(store.get(&name).unwrap(), None);
    }
}
