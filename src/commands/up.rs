//! `up <name>`: build a sandbox, or resume a half-built one of the same name.
//!
//! Acquires the per-name build lock, decides admission against the recorded and
//! live state, creates or reuses the worktree, persists the metadata record
//! before the container starts, then records the anchor's liveness token and
//! provisions networking. This slice is git mode and always detached.

use std::path::PathBuf;

use crate::domain::egress::EgressPolicy;
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName, SandboxRecord};
use crate::domain::policy::{BranchIntent, up_error};
use crate::domain::reconcile::SandboxState;
use crate::ports::{
    Clock, ContainerRegistry, ContainerRuntime, LivenessProbe, MetadataStore, NetworkProvider,
    NetworkSpec, OciSpec, SandboxLock, WorktreeProvider,
};

/// Coordinates building (or resuming) the sandbox named `<name>` over the ports
/// it depends on. Per-sandbox paths derive from `state_root`.
pub struct UpCommand<'a> {
    lock: &'a dyn SandboxLock,
    store: &'a dyn MetadataStore,
    probe: &'a dyn LivenessProbe,
    registry: &'a dyn ContainerRegistry,
    worktrees: &'a dyn WorktreeProvider,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    clock: &'a dyn Clock,
    state_root: PathBuf,
}

impl UpCommand<'_> {
    pub fn run(&self, name: SandboxName, branch: Option<BranchName>) -> Result<(), HortError> {
        if !self.lock.try_acquire(&name)? {
            return Err(HortError::UpInProgress { name: name.as_str().to_string() });
        }

        let sandbox_dir = self.state_root.join("sandboxes").join(name.as_str());
        let worktree_path = sandbox_dir.join(format!("worktree-{}", name.as_str()));
        let overlay_path = sandbox_dir.join("overlay");

        let stored = self.store.get(&name)?;
        let existing = match &stored {
            Some(record) => Some(record.reconcile(self.probe)),
            None => self
                .registry
                .list_live()?
                .iter()
                .any(|entry| entry.id == name)
                .then_some(SandboxState::LostRecord),
        };

        let worktree_listed =
            self.worktrees.list()?.iter().any(|worktree| worktree.path == worktree_path);
        let own = stored.is_some() || worktree_listed;

        let (intent, branch_to_checkout) = match &branch {
            None => {
                let own_branch = BranchName::new(name.as_str())?;
                let branch_taken = self.worktrees.branch_exists(&own_branch)? && !own;
                (BranchIntent::CreateNew { branch_taken }, own_branch)
            }
            Some(target) => {
                let checked_out_elsewhere = self.worktrees.is_checked_out(target)?;
                (
                    BranchIntent::UseExisting { branch: target.clone(), checked_out_elsewhere },
                    target.clone(),
                )
            }
        };

        if let Some(error) = up_error(&name, false, existing, intent) {
            return Err(error);
        }

        if let Some(target) = &branch
            && !self.worktrees.branch_exists(target)?
        {
            return Err(HortError::BranchDoesNotExist {
                branch: target.as_str().to_string(),
                name: name.as_str().to_string(),
            });
        }

        if !worktree_listed {
            self.worktrees.create(&name, &branch_to_checkout)?;
        }

        // Persist the record before the anchor starts: if the container then fails
        // to come up, the half-built sandbox stays recorded so a later run can
        // reconcile and clean it, instead of leaking a worktree nothing tracks.
        let record = match stored {
            Some(record) => record,
            None => {
                let timestamp = humantime::format_rfc3339(self.clock.now()).to_string();
                let fresh = SandboxRecord::new(
                    name.clone(),
                    Some(branch_to_checkout),
                    worktree_path,
                    overlay_path,
                    timestamp.clone(),
                    timestamp,
                    None,
                );
                self.store.put(&fresh)?;
                fresh
            }
        };

        let token = self.runtime.start_anchor(&OciSpec)?;
        self.store.put(&record.with_token(token))?;

        self.network.provision(&NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{}/ns/net", token.pid.0)),
            egress: EgressPolicy::Open,
            db_forwards: Vec::new(),
        })?;

        self.lock.release(&name)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::SystemTime;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode};
    use crate::fakes::{
        FakeNetwork, FakeRegistry, FakeRuntime, FakeSandboxLock, FakeWorktreeProvider,
        InMemoryMetadataStore, ScriptedClock, ScriptedLivenessProbe, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    #[allow(clippy::too_many_arguments)]
    fn up_command<'a>(
        lock: &'a FakeSandboxLock,
        store: &'a InMemoryMetadataStore,
        probe: &'a ScriptedLivenessProbe,
        registry: &'a FakeRegistry,
        worktrees: &'a FakeWorktreeProvider,
        runtime: &'a FakeRuntime,
        network: &'a FakeNetwork,
        clock: &'a ScriptedClock,
    ) -> UpCommand<'a> {
        UpCommand {
            lock,
            store,
            probe,
            registry,
            worktrees,
            runtime,
            network,
            clock,
            state_root: PathBuf::from("/state"),
        }
    }

    #[test]
    fn up_creates_new_branch_named_after_sandbox() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert_eq!(worktrees.creates(), vec![BranchName::new("demo").unwrap()]);
    }

    #[test]
    fn up_persists_metadata_before_starting_container() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::failing_start(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_err());
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap();
        assert_eq!(persisted.unwrap().liveness_token(), None);
    }

    #[test]
    fn up_records_token_after_anchor_starts() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap();
        assert_eq!(persisted.unwrap().liveness_token(), Some(canned_token()));
    }

    #[test]
    fn up_is_reentrant_against_half_built_state() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_existing_branch("demo")
            .with_listed_worktree(&SandboxName::new("demo").unwrap());
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert!(worktrees.creates().is_empty());
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap();
        assert_eq!(persisted.unwrap().liveness_token(), Some(canned_token()));
    }

    #[test]
    fn up_resumes_when_worktree_exists_without_record() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_existing_branch("demo")
            .with_listed_worktree(&SandboxName::new("demo").unwrap());
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert!(worktrees.creates().is_empty());
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap();
        assert_eq!(persisted.unwrap().liveness_token(), Some(canned_token()));
    }

    #[test]
    fn up_errors_branch_exists_for_unowned_existing_branch() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_existing_branch("demo");
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::BranchExists { name: "demo".to_string() }));
    }

    #[test]
    fn concurrent_up_loser_fails_with_in_progress() {
        let lock = FakeSandboxLock::held();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::UpInProgress { name: "demo".to_string() }));
        assert_eq!(store.get(&SandboxName::new("demo").unwrap()).unwrap(), None);
        assert!(worktrees.creates().is_empty());
    }

    #[test]
    fn up_releases_lock_after_build() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert_eq!(lock.releases(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn up_errors_duplicate_for_live_sandbox() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo").with_token(canned_token())).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn up_treats_live_anchor_without_record_as_duplicate() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(true);
        let registry = FakeRegistry::new(vec![(SandboxName::new("demo").unwrap(), canned_token())]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::DuplicateName { name: "demo".to_string() }));
    }

    #[test]
    fn up_targets_existing_branch_with_branch_flag() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().with_existing_branch("feature-x");
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(
            SandboxName::new("demo").unwrap(),
            Some(BranchName::new("feature-x").unwrap()),
        );

        assert!(result.is_ok());
        assert_eq!(worktrees.creates(), vec![BranchName::new("feature-x").unwrap()]);
    }

    #[test]
    fn up_errors_checked_out_for_branch_in_another_worktree() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new()
            .with_existing_branch("feature-x")
            .with_checked_out_branch("feature-x");
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(
            SandboxName::new("demo").unwrap(),
            Some(BranchName::new("feature-x").unwrap()),
        );

        assert_eq!(result, Err(HortError::BranchCheckedOut { branch: "feature-x".to_string() }));
    }

    #[test]
    fn up_errors_branch_does_not_exist_for_missing_branch_target() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(
            SandboxName::new("demo").unwrap(),
            Some(BranchName::new("feature-x").unwrap()),
        );

        assert_eq!(
            result,
            Err(HortError::BranchDoesNotExist {
                branch: "feature-x".to_string(),
                name: "demo".to_string(),
            })
        );
    }

    #[test]
    fn up_provisions_open_network() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let command =
            up_command(&lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock);

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert_eq!(network.provisioned(), vec![SandboxName::new("demo").unwrap()]);
    }
}
