//! `up <name>`: build a sandbox, or resume a half-built one of the same name.
//!
//! Refuses a directory no marker declares a project, checks the host and rootfs
//! preconditions before it takes anything, acquires the per-name build lock,
//! decides admission against the recorded and live state, creates or reuses the
//! worktree (or mounts the project folder itself where there is no git),
//! persists the metadata record before the container starts, then records the
//! anchor's liveness token and provisions networking. Always detached.

use std::path::{Path, PathBuf};

use crate::domain::cache::project_cache_key;
use crate::domain::config::ResolvedConfig;
use crate::domain::egress::{EgressPolicy, egress_degradation_warning};
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName, SandboxRecord, Warning};
use crate::domain::mounts::{cache_mount_plan, declared_read_only_sources, read_only_mount_plan};
use crate::domain::policy::{BranchIntent, up_error};
use crate::domain::preconditions::up_precondition_error;
use crate::domain::reconcile::SandboxState;
use crate::domain::resources::resource_limits;
use crate::ports::{
    CacheProvider, Clock, ContainerRegistry, ContainerRuntime, DbForward, EnvironmentProbe,
    LivenessProbe, MetadataStore, NetworkProvider, NetworkSpec, OciSpec, SandboxLock,
    WorktreeProvider,
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
    cache: &'a dyn CacheProvider,
    state_root: PathBuf,
    /// The project a marker declares, `None` when nothing up the chain declares
    /// one. Without git this directory is what backs `/workdir`, and its absence
    /// is what a build refuses on.
    project_dir: Option<PathBuf>,
    /// Where hort was invoked, which is the directory a refusal names.
    current_dir: PathBuf,
    /// The home the user has on this host. A declared path under it is carried
    /// into the sandbox at the same place under the sandbox's own home, so the
    /// decision cannot be made without knowing where that home starts.
    host_home: PathBuf,
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
        cache: &'a dyn CacheProvider,
        state_root: PathBuf,
        project_dir: Option<PathBuf>,
        current_dir: PathBuf,
        host_home: PathBuf,
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
            cache,
            state_root,
            project_dir,
            current_dir,
            host_home,
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
        // Asked before anything about the host, because it asks a different
        // question: the host checks answer whether sandboxes can be built here at
        // all, this one answers whether hort was given anything to sandbox. It is
        // also what keeps a build off whatever directory the shell happened to be
        // in, so it cannot sit behind checks that need a real kernel to reach.
        let Some(project_dir) = self.project_dir.as_ref() else {
            return Err(HortError::NotAProject { path: self.current_dir.display().to_string() });
        };

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

        let declared = declared_read_only_sources(self.config);
        let (mut mounts, mount_warnings) =
            read_only_mount_plan(&self.env.inspect_mount_sources(&declared), &self.host_home);
        warnings.extend(mount_warnings);
        // Planned after the read-only paths and mounted after them: the list is
        // applied in order, so a cache addressed inside a read-only tree wins,
        // and a cache the box cannot write to is worse than no cache at all.
        let caches = cache_mount_plan(
            self.config,
            &self.state_root.join("cache").join(project_cache_key(project_dir)),
        );

        if !self.lock.try_acquire(&name)? {
            return Err(HortError::UpInProgress { name: name.as_str().to_string() });
        }

        let sandbox_dir = self.state_root.join("sandboxes").join(name.as_str());
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

        // The fork sits above every read below, not just above the worktree
        // creation: listing worktrees is already a git command, and outside a
        // repository it is the first one to fail. Without git the project folder
        // itself is what the sandbox mounts and there is no branch at all.
        let git = self.worktrees.is_git_repo()?;
        let worktree_path = if git {
            sandbox_dir.join(format!("worktree-{}", name.as_str()))
        } else {
            project_dir.clone()
        };
        let worktree_listed =
            git && self.worktrees.list()?.iter().any(|worktree| worktree.path == worktree_path);
        let own = stored.is_some() || worktree_listed;

        let (intent, branch_to_checkout) = match (git, &branch) {
            (false, _) => (BranchIntent::NoGit { branch_flag: branch.is_some() }, None),
            (true, None) => {
                let own_branch = BranchName::new(name.as_str())?;
                let branch_taken = self.worktrees.branch_exists(&own_branch)? && !own;
                (BranchIntent::CreateNew { branch_taken }, Some(own_branch))
            }
            (true, Some(target)) => {
                let checked_out_elsewhere = self.worktrees.is_checked_out(target)?;
                (
                    BranchIntent::UseExisting { branch: target.clone(), checked_out_elsewhere },
                    Some(target.clone()),
                )
            }
        };

        if let Some(error) = up_error(&name, false, existing, intent) {
            return Err(error);
        }

        // Past the selection a branch flag means git mode, because without git the
        // flag was already refused: adding a git test here would be dead, and
        // dropping the selection above would put a git read in the no-git arm.
        if let Some(target) = &branch
            && !self.worktrees.branch_exists(target)?
        {
            return Err(HortError::BranchDoesNotExist {
                branch: target.as_str().to_string(),
                name: name.as_str().to_string(),
            });
        }

        if let Some(target) = &branch_to_checkout
            && !worktree_listed
        {
            self.worktrees.create(&name, target)?;
        }

        // The other mount source hort owns rather than finds, made here for the
        // same reason the worktree is: a bind whose source is missing takes the
        // whole container down, and nothing else on the machine creates these.
        self.cache.ensure(&caches.iter().map(|cache| cache.source.clone()).collect::<Vec<_>>())?;
        mounts.extend(caches);

        // Persist the record before the anchor starts: if the container then fails
        // to come up, the half-built sandbox stays recorded so a later run can
        // reconcile and clean it, instead of leaking a worktree nothing tracks.
        let record = match stored {
            Some(record) => record,
            None => {
                let timestamp = humantime::format_rfc3339(self.clock.now()).to_string();
                let fresh = SandboxRecord::new(
                    name.clone(),
                    branch_to_checkout,
                    worktree_path.clone(),
                    overlay_path.clone(),
                    timestamp.clone(),
                    timestamp,
                    None,
                    project_dir.clone(),
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
            mounts,
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

    use crate::domain::config::{Cache, CacheDir, Egress, Mounts, Network, Resources};
    use crate::domain::model::{AnchorPid, Capabilities, CgroupCaps, LivenessToken, MountNsInode};
    use crate::ports::{MountAccess, SandboxMount};

    use crate::fakes::{
        FakeCacheProvider, FakeCapabilities, FakeNetwork, FakeRegistry, FakeRuntime,
        FakeSandboxLock, FakeWorktreeProvider, InMemoryMetadataStore, ScriptedClock,
        ScriptedLivenessProbe, sample_record,
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
        cache: &'a FakeCacheProvider,
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
            cache,
            state_root: PathBuf::from("/state"),
            project_dir: Some(PathBuf::from("/project")),
            current_dir: PathBuf::from("/project"),
            host_home: PathBuf::from("/home/tester"),
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(runtime.started_memory_bytes(), Some(4_294_967_296));
        assert_eq!(runtime.started_cpus(), Some(2.0));
    }

    #[test]
    fn up_puts_the_declared_read_only_mounts_in_the_container_spec() {
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
            mounts: Mounts { read_only: vec!["/home/tester/.config/fish".to_string()] },
            ..healthy_config()
        };
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // A plan nobody hands to the runtime builds the same empty box as no
        // plan at all, and every test of the planning itself stays green while
        // it happens.
        assert_eq!(
            runtime.started_mounts(),
            vec![SandboxMount {
                source: PathBuf::from("/home/tester/.config/fish"),
                target: PathBuf::from("/home/hort/.config/fish"),
                access: MountAccess::ReadOnly,
            }]
        );
    }

    #[test]
    fn two_sandboxes_of_one_project_share_a_cache_dir() {
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
            cache: Cache { dirs: vec![CacheDir::Path("node_modules".to_string())] },
            ..healthy_config()
        };
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );
        let shared = SandboxMount {
            source: PathBuf::from("/state/cache/%2Fproject/node_modules"),
            target: PathBuf::from("/workdir/node_modules"),
            access: MountAccess::ReadWrite,
        };

        command.run(SandboxName::new("first").unwrap(), None).unwrap();
        let first = runtime.started_mounts();
        command.run(SandboxName::new("second").unwrap(), None).unwrap();

        // The cache belongs to the project, not to the box: what took four
        // minutes to populate is the reason to throw a sandbox away without
        // thinking twice, and a cache keyed by sandbox would make the second box
        // of a project pay that price over again. The address is the witness,
        // because keying it by anything else is invisible until someone waits.
        assert_eq!(first, vec![shared.clone()]);
        assert_eq!(runtime.started_mounts(), vec![shared]);
    }

    #[test]
    fn up_creates_the_cache_directories_before_the_container_starts() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::failing_start(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = ResolvedConfig {
            cache: Cache { dirs: vec![CacheDir::Path("node_modules".to_string())] },
            ..healthy_config()
        };
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // A bind whose source is not on the host takes the whole container down,
        // and this source is one hort chose the address of and nobody else ever
        // creates, so the first run of every project would be the run that never
        // boots. The container failing to start is what dates the creation: it
        // happened, and it happened before there was a container.
        assert!(result.is_err());
        assert_eq!(cache.ensured(), vec![PathBuf::from("/state/cache/%2Fproject/node_modules")]);
    }

    #[test]
    fn a_cache_comes_after_the_read_only_mount_it_lands_inside() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        // The whole configuration directory carried in read-only, and one
        // directory inside it declared a cache, which is what a user does for a
        // tool that writes into its own configuration directory and wants those
        // writes to outlive the box.
        let config = ResolvedConfig {
            mounts: Mounts { read_only: vec!["/home/tester/.config".to_string()] },
            cache: Cache {
                dirs: vec![CacheDir::Named {
                    name: "fish".to_string(),
                    target: "~/.config/fish".to_string(),
                }],
            },
            ..healthy_config()
        };
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The mount list is applied in order, so of two mounts whose
        // destinations overlap the box only ever sees the later one. Carried the
        // other way round, the read-only bind lands on top of the cache and the
        // box gets a cache it cannot write to, which is worse than having no
        // cache at all: the tool now fails where it used to merely reinstall.
        // Nothing else in the build decides this, since the two plans are made
        // apart and never see each other.
        assert_eq!(
            runtime.started_mounts(),
            vec![
                SandboxMount {
                    source: PathBuf::from("/home/tester/.config"),
                    target: PathBuf::from("/home/hort/.config"),
                    access: MountAccess::ReadOnly,
                },
                SandboxMount {
                    source: PathBuf::from("/state/cache/%2Fproject/fish"),
                    target: PathBuf::from("/home/hort/.config/fish"),
                    access: MountAccess::ReadWrite,
                },
            ]
        );
    }

    #[test]
    fn up_returns_the_warning_for_a_source_the_host_does_not_have() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host()).with_missing_mount_sources();
        let config = ResolvedConfig {
            mounts: Mounts { read_only: vec!["/home/tester/.tmux.conf".to_string()] },
            ..healthy_config()
        };
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The build survives the missing path either way, so a warning that
        // stops at the planner leaves the user in a box quietly missing what
        // they asked for.
        assert!(
            warnings.iter().any(|warning| warning.to_string().contains("/home/tester/.tmux.conf"))
        );
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(network.provisioned_forwards(), vec![("127.0.0.1".to_string(), 5432)]);
    }

    #[test]
    fn up_builds_from_the_project_folder_without_git() {
        let project = PathBuf::from("/home/tester/project");
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project.clone(),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert_eq!(runtime.started_workdir(), project);
    }

    #[test]
    fn up_records_no_branch_without_git() {
        let project = PathBuf::from("/home/tester/project");
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // A null branch is what every later reader keys on, teardown included, so
        // a sandbox that loses it is one whose teardown starts deleting the
        // user's own folder.
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap().unwrap();
        assert_eq!(persisted.branch(), None);
    }

    #[test]
    fn up_creates_no_worktree_without_git() {
        let project = PathBuf::from("/home/tester/project");
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        assert!(worktrees.creates().is_empty());
    }

    #[test]
    fn up_refuses_a_directory_that_is_not_a_project() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: None,
            current_dir: PathBuf::from("/home/tester/Downloads"),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // What authorizes hort to hand a host folder to a sandbox running an
        // agent with no permission limits is the user having left a marker file
        // there, never where the shell happened to be.
        assert_eq!(
            result,
            Err(HortError::NotAProject { path: "/home/tester/Downloads".to_string() })
        );
    }

    #[test]
    fn up_reports_branch_requires_git_for_the_branch_flag_without_git() {
        let project = PathBuf::from("/home/tester/project");
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        let result = command
            .run(SandboxName::new("demo").unwrap(), Some(BranchName::new("feature-x").unwrap()));

        assert_eq!(result, Err(HortError::BranchRequiresGit));
    }

    #[test]
    fn up_refuses_a_directory_that_is_not_a_project_even_with_the_branch_flag() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: None,
            current_dir: PathBuf::from("/home/tester/Downloads"),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        let result = command
            .run(SandboxName::new("demo").unwrap(), Some(BranchName::new("feature-x").unwrap()));

        // Whether a branch flag is legal here is a question about a project, and
        // there is no project to ask it about.
        assert_eq!(
            result,
            Err(HortError::NotAProject { path: "/home/tester/Downloads".to_string() })
        );
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        assert!(result.is_err());
        assert!(runtime.started_env().is_empty());
    }

    #[test]
    fn up_records_the_project_the_sandbox_was_built_from() {
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
        let cache = FakeCacheProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // With git the worktree is one hort made under its own state root, so
        // the record is the only place the project the sandbox came from is
        // written down, and the cache of that project is keyed by it.
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap().unwrap();
        assert_eq!(persisted.project_path(), Some(Path::new("/project")));
    }

    #[test]
    fn up_records_the_project_without_git() {
        let project = PathBuf::from("/home/tester/project");
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new().no_git();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project.clone(),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &config,
            )
        };

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // Here the project folder is also the worktree, so writing it twice
        // looks redundant. It is not: a reader that had to know which mode a
        // record is in before it could find the project would be a second rule,
        // and the guard that reads this field treats a record that cannot answer
        // as one to protect against.
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap().unwrap();
        assert_eq!(persisted.project_path(), Some(project.as_path()));
    }
}
