//! `up <name>`: build a sandbox, or resume a half-built one of the same name.
//!
//! Checks the host and rootfs preconditions before it takes anything, acquires
//! the per-name build lock, decides admission against the recorded and live
//! state, creates or reuses the worktree, persists the metadata record before the
//! container starts, then records the anchor's liveness token and provisions
//! networking. This slice is git mode and always detached.

use std::path::{Path, PathBuf};

use crate::domain::config::ResolvedConfig;
use crate::domain::egress::{EgressPolicy, egress_degradation_warning};
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName, SandboxRecord, Warning};
use crate::domain::policy::{BranchIntent, up_error};
use crate::domain::preconditions::up_precondition_error;
use crate::domain::reconcile::SandboxState;
use crate::domain::resources::resource_limits;
use crate::ports::{
    Clock, ContainerRegistry, ContainerRuntime, DbForward, EnvironmentProbe, LivenessProbe,
    MetadataStore, NetworkProvider, NetworkSpec, OciSpec, SandboxLock, WorktreeProvider,
};

/// Coordinates building (or resuming) the sandbox named `<name>` over the ports
/// it depends on. Per-sandbox paths derive from `state_root`; what the sandbox
/// is made of comes from the resolved configuration.
pub struct UpCommand<'a> {
    lock: &'a dyn SandboxLock,
    store: &'a dyn MetadataStore,
    probe: &'a dyn LivenessProbe,
    registry: &'a dyn ContainerRegistry,
    worktrees: &'a dyn WorktreeProvider,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    clock: &'a dyn Clock,
    env: &'a dyn EnvironmentProbe,
    state_root: PathBuf,
    config: &'a ResolvedConfig,
}

impl<'a> UpCommand<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lock: &'a dyn SandboxLock,
        store: &'a dyn MetadataStore,
        probe: &'a dyn LivenessProbe,
        registry: &'a dyn ContainerRegistry,
        worktrees: &'a dyn WorktreeProvider,
        runtime: &'a dyn ContainerRuntime,
        network: &'a dyn NetworkProvider,
        clock: &'a dyn Clock,
        env: &'a dyn EnvironmentProbe,
        state_root: PathBuf,
        config: &'a ResolvedConfig,
    ) -> Self {
        Self {
            lock,
            store,
            probe,
            registry,
            worktrees,
            runtime,
            network,
            clock,
            env,
            state_root,
            config,
        }
    }
}

impl UpCommand<'_> {
    /// Build (or resume) the sandbox, returning the advisories the user has to
    /// hear: a resource cap the host cannot enforce, and whatever later slices
    /// degrade rather than fail on.
    pub fn run(
        &self,
        name: SandboxName,
        branch: Option<BranchName>,
    ) -> Result<Vec<Warning>, HortError> {
        let host = self.env.detect();
        let rootfs_facts = self.config.rootfs.as_deref().map(|configured| {
            self.env.inspect_rootfs(Path::new(configured), self.config.shell.as_deref())
        });
        // The posture is resolved first because it decides what the host has to
        // provide: only an allowlist needs the tooling that empties the sandbox's
        // route tables.
        let egress = EgressPolicy::from_config(self.config.egress.as_ref())?;
        if let Some(error) = up_precondition_error(&host, &egress, rootfs_facts.as_ref()) {
            return Err(error);
        }
        // The selection is handed an Option so its order holds: a host without
        // user namespaces has to say so rather than blame a missing rootfs. Past
        // it the facts are always there, and their path is the inspected one.
        let rootfs = rootfs_facts.ok_or(HortError::NoRootfsConfigured)?.path;

        // Everything the configuration can get wrong is read before the build
        // lock is taken: failing fast means failing before holding a resource.
        let (limits, mut warnings) = resource_limits(self.config.resources.as_ref(), &host.cgroup)?;
        // Said here rather than by the sessions that run degraded, because by the
        // time one of those starts the sandbox exists and the user is inside it.
        warnings.extend(egress_degradation_warning(&egress, host.landlock_abi));

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
                    worktree_path.clone(),
                    overlay_path.clone(),
                    timestamp.clone(),
                    timestamp,
                    None,
                );
                self.store.put(&fresh)?;
                fresh
            }
        };

        let worktree_display = worktree_path.display().to_string();
        let token = self.runtime.start_anchor(&OciSpec {
            name: name.clone(),
            rootfs,
            overlay: overlay_path,
            workdir: worktree_path,
            env: vec![
                ("HORT_SANDBOX".to_string(), name.as_str().to_string()),
                ("HORT_WORKTREE".to_string(), worktree_display),
            ],
            resources: limits,
        })?;
        self.store.put(&record.with_token(token))?;

        self.network.provision(&NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{}/ns/net", token.pid.0)),
            egress,
            db_forwards: self
                .config
                .network
                .iter()
                .map(|database| DbForward { host: database.host.clone(), port: database.port })
                .collect(),
        })?;

        self.lock.release(&name)?;
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::SystemTime;

    use crate::domain::config::{Cache, Egress, Mounts, Network, Resources};
    use crate::domain::model::{AnchorPid, Capabilities, CgroupCaps, LivenessToken, MountNsInode};
    use crate::fakes::{
        FakeCapabilities, FakeNetwork, FakeRegistry, FakeRuntime, FakeSandboxLock,
        FakeWorktreeProvider, InMemoryMetadataStore, ScriptedClock, ScriptedLivenessProbe,
        sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: true,
            git: true,
        }
    }

    fn healthy_config() -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some("/base/rootfs".to_string()),
            agents: Vec::new(),
            mounts: Mounts::default(),
            network: Vec::new(),
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: None,
            resources: None,
        }
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
        env: &'a FakeCapabilities,
        config: &'a ResolvedConfig,
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
            env,
            state_root: PathBuf::from("/state"),
            config,
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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_err());
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap();
        assert_eq!(persisted.unwrap().liveness_token(), None);
    }

    #[test]
    fn up_hands_the_runtime_the_sandbox_environment() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert!(runtime.started_env().contains(&("HORT_SANDBOX".to_string(), "demo".to_string())));
        assert!(runtime.started_env().contains(&(
            "HORT_WORKTREE".to_string(),
            "/state/sandboxes/demo/worktree-demo".to_string()
        )));
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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command
            .run(SandboxName::new("demo").unwrap(), Some(BranchName::new("feature-x").unwrap()));

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command
            .run(SandboxName::new("demo").unwrap(), Some(BranchName::new("feature-x").unwrap()));

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command
            .run(SandboxName::new("demo").unwrap(), Some(BranchName::new("feature-x").unwrap()));

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
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_ok());
        assert_eq!(network.provisioned(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn up_reports_the_precondition_error_over_a_held_lock() {
        let lock = FakeSandboxLock::held();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { user_ns: false, ..ready_host() });
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn up_errors_when_an_allowlist_needs_ip_and_the_host_lacks_it() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { ip: None, ..ready_host() });
        let config = ResolvedConfig {
            egress: Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::IpMissing));
    }

    #[test]
    fn up_rejects_a_rootfs_the_host_reports_missing() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host()).with_missing_rootfs();
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert_eq!(result, Err(HortError::RootfsMissing { path: "/base/rootfs".to_string() }));
    }

    #[test]
    fn up_passes_the_configured_shell_to_the_rootfs_inspection() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config =
            ResolvedConfig { shell: Some("/usr/bin/fish".to_string()), ..healthy_config() };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(
            env.inspections(),
            vec![(PathBuf::from("/base/rootfs"), Some("/usr/bin/fish".to_string()))]
        );
    }

    #[test]
    fn up_puts_the_configured_rootfs_in_the_container_spec() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(runtime.started_rootfs(), PathBuf::from("/base/rootfs"));
    }

    #[test]
    fn up_puts_the_resource_ceiling_in_the_container_spec() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = ResolvedConfig {
            resources: Some(Resources { memory: Some("4g".to_string()), cpus: Some(2.0) }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(runtime.started_memory_bytes(), Some(4_294_967_296));
        assert_eq!(runtime.started_cpus(), Some(2.0));
    }

    #[test]
    fn up_returns_the_degradation_warning_when_a_controller_is_missing() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities {
            cgroup: CgroupCaps { memory: false, pids: true, cpu: true, cpuset: false },
            ..ready_host()
        });
        let config = ResolvedConfig {
            resources: Some(Resources { memory: Some("4g".to_string()), cpus: None }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert!(warnings.iter().any(|warning| warning.to_string().contains("memory")));
    }

    #[test]
    fn up_warns_when_the_host_cannot_enforce_the_allowlist_in_the_kernel() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { landlock_abi: None, ..ready_host() });
        let config = ResolvedConfig {
            egress: Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The kernel drops what it cannot enforce and reports success, so a
        // sandbox built here runs one layer thinner than the one that was asked
        // for. Said once per sandbox, here, because a session cannot say it: by
        // then the box exists and the user is inside it.
        assert!(!warnings.is_empty());
    }

    #[test]
    fn up_is_silent_when_the_host_can_enforce_the_allowlist_in_the_kernel() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { landlock_abi: Some(4), ..ready_host() });
        let config = ResolvedConfig {
            egress: Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // An advisory raised whatever the host reports is one the user learns to
        // scroll past, and the next one that matters scrolls past with it. This
        // is what keeps the one above tied to the kernel it is about.
        assert!(warnings.is_empty());
    }

    #[test]
    fn up_carries_the_configured_egress_policy_into_the_network_spec() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = ResolvedConfig {
            egress: Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        let policy = network.provisioned_egress().expect("up provisions the sandbox network");
        assert!(policy.matches("api.anthropic.com"));
        assert!(!policy.matches("evil.com"));
    }

    #[test]
    fn up_carries_the_declared_database_forwards_into_the_network_spec() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = ResolvedConfig {
            network: vec![Network {
                mode: "host".to_string(),
                host: "127.0.0.1".to_string(),
                port: 5432,
            }],
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(network.provisioned_forwards(), vec![("127.0.0.1".to_string(), 5432)]);
    }

    #[test]
    fn up_rejects_a_malformed_egress_allowlist_before_starting_the_container() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = ResolvedConfig {
            egress: Some(Egress::Allowlist { allow: vec!["not a host!".to_string()] }),
            ..healthy_config()
        };
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_err());
        assert!(runtime.started_env().is_empty());
    }
}
