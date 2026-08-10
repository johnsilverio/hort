//! `prune`: explicit, confirmed cleanup of idle sandboxes and abrupt-death
//! debris (orphaned and inconsistent sandboxes, corrupt metadata dirs, and the
//! caches of projects that are gone).
//!
//! It reconciles the records against the live anchors and the worktrees still on
//! disk, gathers the corrupt dirs and the stored caches, observes idle, worktree
//! risk and project presence, and runs the pure selection. Every question about a
//! worktree is asked at that candidate's own path on disk, because `prune` is
//! global and the repository the user happens to be standing in knows nothing
//! about the other projects' sandboxes; the same holds for a cache, asked at the
//! project path its key decodes to. An empty removal set never prompts;
//! otherwise, without `--force`, a non-TTY stdin refuses and a TTY prompts with
//! every candidate name listed first. Each chosen removal follows the mandatory
//! teardown order, the caches coming last of all, and a single stale-registration
//! sweep runs at the end of every non-refused, non-declined run.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::commands::present_worktrees;
use crate::domain::cache::project_from_cache_key;
use crate::domain::error::HortError;
use crate::domain::idle::{IdleState, idle, parse_timestamp};
use crate::domain::model::{SandboxName, SandboxRecord};
use crate::domain::prune::{
    CacheInput, CacheRisk, CorruptInput, PruneInput, PrunePlan, PruneSkip, WorktreeRisk,
    prune_selection,
};
use crate::domain::reconcile::reconcile_all;
use crate::domain::teardown::{TeardownStep, teardown_plan};
use crate::ports::{
    CacheProvider, Clock, Confirmer, ContainerRegistry, ContainerRuntime, MetadataStore,
    NetworkProvider, SessionProbe, WorktreeProvider,
};

/// What a `prune` run removed and what it skipped, with the reason for each skip.
///
/// Collected caches are a list of their own, and they name the project each one
/// belonged to rather than the key it is stored under: a project path and a
/// sandbox name in one list cannot be told apart by whoever reads the output, and
/// the key is an internal address while the project is what the user recognizes.
pub struct PruneReport {
    pub removed: Vec<String>,
    pub removed_caches: Vec<String>,
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
    caches: &'a dyn CacheProvider,
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
        caches: &'a dyn CacheProvider,
        state_root: PathBuf,
    ) -> Self {
        Self {
            store,
            registry,
            worktrees,
            sessions,
            clock,
            confirmer,
            runtime,
            network,
            caches,
            state_root,
        }
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
        let corrupt = self.store.list_corrupt()?;
        let now = self.clock.now();

        let present = present_worktrees(self.worktrees, &records);

        let inputs: Vec<PruneInput> = reconcile_all(&records, &live, &present)
            .into_iter()
            .filter_map(|(name, state)| {
                let record = records.iter().find(|record| record.name() == &name)?;
                let idle = self.sandbox_idle(record, now);
                let risk = self.sandbox_risk(record);
                Some(PruneInput { name, state, idle, risk })
            })
            .collect();

        let corrupt_inputs: Vec<CorruptInput> = corrupt
            .iter()
            .map(|entry| CorruptInput {
                name: entry.name.clone(),
                risk: self.corrupt_risk(&entry.name),
            })
            .collect();

        let cache_inputs = self.cache_inputs()?;

        let plan = prune_selection(&inputs, &corrupt_inputs, &cache_inputs, idle_threshold, force);

        // A cache counts as work. Left out of this question, a run whose only
        // candidate is an orphaned cache leaves without asking and without
        // removing, reporting that it found nothing.
        if plan.sandboxes.is_empty() && plan.corrupt.is_empty() && plan.caches.is_empty() {
            self.worktrees.prune_stale()?;
            return Ok(PruneReport {
                removed: Vec::new(),
                removed_caches: Vec::new(),
                skipped: plan.skipped,
            });
        }

        if !force {
            if !stdin_is_tty {
                return Err(HortError::RefusedWithoutConfirmation { command: "prune".to_string() });
            }
            if !self.confirmer.confirm(&confirmation_message(&plan))? {
                return Ok(PruneReport {
                    removed: Vec::new(),
                    removed_caches: Vec::new(),
                    skipped: Vec::new(),
                });
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

        // Last, after every sandbox is down. A cache is a writable bind source,
        // so collecting one while a container still holds it pulls the ground out
        // from under a live mount; and a removal that fails here must not have
        // left the metadata behind, or the sandbox comes back as a candidate for
        // every run after this one.
        let mut removed_caches = Vec::new();
        for key in &plan.caches {
            self.caches.remove(key)?;
            removed_caches.push(decoded_project(key));
        }

        self.worktrees.prune_stale()?;
        Ok(PruneReport { removed, removed_caches, skipped: plan.skipped })
    }

    /// Every stored cache with the presence of the project that filled it. The
    /// listing is the port's; the question is asked once per key, of the disk.
    fn cache_inputs(&self) -> Result<Vec<CacheInput>, HortError> {
        Ok(self
            .caches
            .list()?
            .into_iter()
            .map(|key| {
                let risk = self.cache_risk(&key);
                CacheInput { key, risk }
            })
            .collect())
    }

    /// Whether a stored cache's project is still on disk, in the shape the guard
    /// reads. A key that decodes to anything but an absolute path was not written
    /// by hort, which addresses every cache by its project's absolute path, and
    /// removing a directory somebody else put there is the expensive way to be
    /// wrong. A presence read that failed says nothing about the project either,
    /// so both stay unknown, and unknown protects.
    fn cache_risk(&self, key: &str) -> CacheRisk {
        let project = project_from_cache_key(key);
        if !project.is_absolute() {
            return CacheRisk::Unknown;
        }
        match self.caches.project_exists(&project) {
            Ok(true) => CacheRisk::ProjectLives,
            Ok(false) => CacheRisk::NothingAtRisk,
            Err(_) => CacheRisk::Unknown,
        }
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

    /// What a record's worktree holds, asked only when there is something to
    /// protect: without git the folder is the user's own and is never removed,
    /// and a worktree already gone from disk has nothing left to lose, which is
    /// what keeps that debris prunable. Presence is read from the disk at the
    /// record's own path, so a sandbox is judged by what is there rather than by
    /// whether the current repository happens to list it. A probe git cannot
    /// answer leaves the state unknown, and unknown protects.
    fn sandbox_risk(&self, record: &SandboxRecord) -> WorktreeRisk {
        if record.branch().is_none() {
            return WorktreeRisk::NothingAtRisk;
        }
        if !self.worktrees.exists(record.worktree_path()) {
            return WorktreeRisk::NothingAtRisk;
        }
        self.probed_risk(record.name())
    }

    /// What a corrupt dir's worktree holds. A corrupt entry has no record to read
    /// the path from, so the canonical state-root layout supplies it; from there
    /// the question and its answers are the record path's.
    fn corrupt_risk(&self, name: &str) -> WorktreeRisk {
        if !self.worktrees.exists(&self.corrupt_worktree_path(name)) {
            return WorktreeRisk::NothingAtRisk;
        }
        let Ok(sandbox_name) = SandboxName::new(name) else {
            return WorktreeRisk::NothingAtRisk;
        };
        self.probed_risk(&sandbox_name)
    }

    /// The dirty probe read as risk. It lives in one place because the two paths
    /// that ask it gate the same deletion, and a mapping that drifts between them
    /// reopens the data-loss path on whichever side was left behind.
    fn probed_risk(&self, name: &SandboxName) -> WorktreeRisk {
        match self.worktrees.is_dirty(name) {
            Ok(true) => WorktreeRisk::HoldsWork,
            Ok(false) => WorktreeRisk::NothingAtRisk,
            Err(_) => WorktreeRisk::Unknown,
        }
    }

    fn corrupt_worktree_path(&self, name: &str) -> PathBuf {
        self.state_root.join("sandboxes").join(name).join(format!("worktree-{name}"))
    }
}

/// A confirmation message naming every candidate: sandboxes, corrupt dirs, and
/// the caches by the project each one belongs to. The wording is not a product
/// guarantee; only that the names are present. A prompt that named less than the
/// run removes is a yes the user never gave for the rest.
fn confirmation_message(plan: &PrunePlan) -> String {
    let mut names: Vec<String> =
        plan.sandboxes.iter().map(|name| name.as_str().to_string()).collect();
    names.extend(plan.corrupt.iter().cloned());
    names.extend(plan.caches.iter().map(|key| decoded_project(key)));
    format!("prune {}? this removes their worktrees, metadata and caches", names.join(", "))
}

/// The project a stored cache belongs to, which is what every line a person reads
/// names. The key is only ever an address to remove by.
fn decoded_project(key: &str) -> String {
    project_from_cache_key(key).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    use crate::domain::model::{
        AnchorPid, BranchName, LivenessToken, MountNsInode, SandboxName, SandboxRecord,
    };
    use crate::domain::prune::SkipReason;
    use crate::fakes::{
        FakeCacheProvider, FakeConfirmer, FakeNetwork, FakeRegistry, FakeRuntime, FakeSessionProbe,
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
        caches: &'a FakeCacheProvider,
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
            caches,
            state_root: state_root(),
        }
    }

    #[test]
    fn prune_spares_a_live_sandbox_whose_worktree_this_repository_does_not_list() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_present_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, true, false).unwrap();

        // A sandbox reads as debris the moment its worktree looks gone, and
        // answering that from the current repository's list makes every other
        // project's box look exactly that way. What spares this one is
        // reconciling as live, not the risk guard: with no idle threshold a live
        // sandbox is never a candidate for the guard to see.
        assert!(report.removed.is_empty());
    }

    #[test]
    fn prune_removes_orphaned_sandbox_in_teardown_order() {
        let name = SandboxName::new("demo").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, false).unwrap();

        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Dirty }]
        );
        assert!(store.get(&name).unwrap().is_some());
    }

    #[test]
    fn prune_skips_a_sandbox_whose_worktree_state_it_cannot_determine() {
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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, false).unwrap();

        // Calling this one dirty sends the user hunting for uncommitted changes
        // that git can no longer enumerate at all, which is the wrong thing to
        // read right before deciding to pass --force.
        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }]
        );
    }

    #[test]
    fn prune_spares_a_dirty_sandbox_whose_worktree_this_repository_does_not_list() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_present_worktree(&name).with_dirty_worktree(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, true).unwrap();

        // A reboot leaves every sandbox on the machine orphaned, so this is what
        // Monday morning looks like from any other project: uncommitted work one
        // confirmation away from being torn down, and nothing in the report to
        // say it was ever at risk.
        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Dirty }]
        );
    }

    #[test]
    fn prune_spares_a_dirty_corrupt_entry_whose_worktree_this_repository_does_not_list() {
        let rotten = SandboxName::new("rotten").unwrap();
        let store = InMemoryMetadataStore::new().with_corrupt_entry("rotten", "broken json");
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_present_worktree(&rotten).with_dirty_worktree(&rotten);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, true).unwrap();

        assert_eq!(
            report.skipped,
            vec![PruneSkip { name: "rotten".to_string(), reason: SkipReason::Dirty }]
        );
    }

    #[test]
    fn prune_removes_a_sandbox_whose_worktree_vanished_from_disk() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new().with_failing_dirty_probe(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, false, true).unwrap();

        // Debris whose worktree is already gone is exactly what prune exists to
        // clear, and git has nothing left to answer about it. A guard that asks
        // anyway reads the failure as a reason to protect and keeps the debris
        // forever.
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn prune_removes_a_no_git_sandbox_whose_folder_is_not_a_repository() {
        let name = SandboxName::new("demo").unwrap();
        let record = SandboxRecord::new(
            name.clone(),
            None,
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-12T12:00:00Z".to_string(),
            "2026-06-12T12:00:00Z".to_string(),
            None,
        );
        let store = InMemoryMetadataStore::new();
        store.put(&record).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_present_worktree(&name)
            .with_failing_dirty_probe(&name);
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, false, true).unwrap();

        // Without git that folder is the user's own and prune never removes it,
        // so whatever it holds is not this guard's business.
        assert_eq!(store.get(&name).unwrap(), None);
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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

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
        let caches = FakeCacheProvider::new();
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(Some(Duration::from_secs(1800)), true, false).unwrap();

        assert_eq!(store.get(&name).unwrap(), None);
    }

    /// The stored address of a project that is no longer on disk, and the project
    /// it decodes back to.
    const GONE_CACHE_KEY: &str = "%2Fhome%2Ftester%2Fprojects%2Fgone";
    const GONE_PROJECT: &str = "/home/tester/projects/gone";

    /// The stored address of a project the user still works on, and that project.
    const LIVE_CACHE_KEY: &str = "%2Fhome%2Ftester%2Fprojects%2Fhort";
    const LIVE_PROJECT: &str = "/home/tester/projects/hort";

    #[test]
    fn prune_removes_the_cache_directory_of_a_gone_project() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new().with_stored_key(GONE_CACHE_KEY);
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, true, false).unwrap();

        // Nothing else on the machine ever collects these, so a run whose only
        // work is a cache has to do it: the directory hort left behind belongs
        // to a project that will never fill it again.
        assert_eq!(caches.removed(), vec![GONE_CACHE_KEY.to_string()]);
    }

    #[test]
    fn prune_reports_the_cache_it_removed() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new().with_stored_key(GONE_CACHE_KEY);
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, true, false).unwrap();

        // The key is an address hort invented; the project is the thing the
        // person reading this line recognizes as theirs.
        assert_eq!(report.removed_caches, vec![GONE_PROJECT.to_string()]);
    }

    #[test]
    fn prune_prompts_before_removing_a_cache_alone() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new().with_stored_key(GONE_CACHE_KEY);
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, false, true).unwrap();

        // A run with no sandbox and no corrupt dir still has work to do, and one
        // that leaves early takes the confirmation with it: the user is told
        // nothing was found while a directory sat there waiting to be collected.
        assert_eq!(confirmer.prompts().len(), 1);
    }

    #[test]
    fn prune_names_the_cache_it_would_remove_in_the_confirmation_prompt() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new().with_stored_key(GONE_CACHE_KEY);
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, false, true).unwrap();

        // Listing what would go is what the confirmation is for. A prompt that
        // asks about nothing in particular is a yes the user never really gave.
        assert!(confirmer.prompts().concat().contains("gone"));
    }

    #[test]
    fn prune_spares_the_cache_of_a_project_that_is_still_there() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new()
            .with_stored_key(LIVE_CACHE_KEY)
            .with_live_project(Path::new(LIVE_PROJECT));
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, true).unwrap();

        // This is the arm that costs the user four minutes of installing every
        // time it is wrong, and it is wrong for every project on the machine at
        // once if the presence read is read backwards.
        assert!(caches.removed().is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::LiveProject);
    }

    #[test]
    fn prune_skips_a_cache_directory_it_did_not_write() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        // hort addresses a cache by the encoded absolute path of its project, so
        // a name that decodes to anything else is a directory somebody else put
        // under hort's state, and removing what you did not write is the
        // expensive way to be wrong.
        let caches = FakeCacheProvider::new().with_stored_key("scratch");
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, true).unwrap();

        assert!(caches.removed().is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::UnknownProject);
    }

    #[test]
    fn prune_spares_a_cache_it_could_not_ask_the_disk_about() {
        let store = InMemoryMetadataStore::new();
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let caches = FakeCacheProvider::new()
            .with_stored_key(LIVE_CACHE_KEY)
            .with_failing_project_probe(Path::new(LIVE_PROJECT));
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        let report = command.run(None, false, true).unwrap();

        // A read that failed says nothing about whether the project is there, and
        // the cheap way to be wrong here is to let the failure fall through to
        // the removable answer, which collects the cache of every project on a
        // machine where the read happens to be failing.
        assert!(caches.removed().is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::UnknownProject);
    }

    #[test]
    fn prune_removes_a_cache_only_after_the_sandbox_teardowns() {
        let name = SandboxName::new("demo").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let registry = FakeRegistry::new(vec![]);
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let clock = ScriptedClock::new(std::time::SystemTime::UNIX_EPOCH);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let caches =
            FakeCacheProvider::new().with_stored_key(GONE_CACHE_KEY).with_trace(trace.clone());
        let command = prune_command(
            &store, &registry, &worktrees, &sessions, &clock, &confirmer, &runtime, &network,
            &caches,
        );

        command.run(None, true, false).unwrap();

        // A cache is a writable bind source, so a run that collects it before the
        // container holding it is gone pulls the ground out from under a live
        // mount, which is the one thing the teardown order exists to forbid.
        let expected = vec![
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
            "worktrees.remove".to_string(),
            "store.remove".to_string(),
            "cache.remove".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }
}
