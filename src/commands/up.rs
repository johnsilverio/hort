//! `up <name>`: build a sandbox, or resume a half-built one of the same name.
//!
//! Refuses a directory no marker declares a project, checks the host and rootfs
//! preconditions, and refuses a cache aimed at a path the read-only mount
//! covering it does not have, all before it takes anything. Then it acquires
//! the per-name build lock, decides admission against the recorded and live
//! state, creates or reuses the worktree (or mounts the project folder itself
//! where there is no git), and persists the metadata record before the
//! container starts, then records the anchor's liveness token, provisions
//! networking and starts the watcher that turns a completion written inside the
//! box into a notification on the host. A failure once the anchor is standing
//! undoes the build, stopping the host-side helpers and then the container while
//! leaving the worktree and the record for the next run. The watcher is the one
//! step that degrades instead: it warns and the sandbox stays up. Always
//! detached.

use std::path::{Path, PathBuf};

use crate::domain::cache::project_cache_key;
use crate::domain::config::ResolvedConfig;
use crate::domain::egress::{EgressPolicy, egress_degradation_warning};
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName, SandboxRecord, Warning};
use crate::domain::mounts::{
    cache_landing_error, cache_landings, cache_mount_plan, declared_read_only_sources,
    read_only_mount_plan,
};
use crate::domain::notify::{
    channel_is_declared, channel_mount, channel_sink, notify_degradation_warning,
    render_notification, stop_hook_drop_ins,
};
use crate::domain::policy::{BranchIntent, up_error};
use crate::domain::preconditions::up_precondition_error;
use crate::domain::reconcile::SandboxState;
use crate::domain::resources::resource_limits;
use crate::domain::teardown::{TeardownStep, rollback_plan};
use crate::ports::{
    CacheProvider, Clock, ContainerRegistry, ContainerRuntime, DbForward, EnvironmentProbe,
    LivenessProbe, MetadataStore, NetworkProvider, NetworkSpec, NotifyProvider, NotifySpec,
    OciSpec, SandboxLock, WorktreeProvider,
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
    notify: &'a dyn NotifyProvider,
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
        notify: &'a dyn NotifyProvider,
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
            notify,
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

        // A host that cannot raise a completion still builds the box: what a
        // refusal here would cost is work nobody else has a copy of, and what
        // this costs is a desktop popup. The program is the one the probe found,
        // carried to the watcher rather than looked up again in a process that
        // can no longer ask anything.
        let sink = channel_sink(self.config);
        let raising = match &sink {
            Some(configured) => {
                match notify_degradation_warning(configured, host.notify_send.as_deref()) {
                    Some(mute) => {
                        warnings.push(mute);
                        None
                    }
                    None => host.notify_send.clone(),
                }
            }
            None => None,
        };

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
        // Asked here, above the lock and everything it guards, because the answer
        // is about what the configuration says and nothing built yet is part of
        // it: a refusal that arrived after the worktree would leave a directory
        // git has registered, no record names, and no sweep collects.
        let landings = cache_landings(&mounts, &caches);
        let landing_paths: Vec<PathBuf> =
            landings.iter().map(|landing| landing.host_path.clone()).collect();
        if let Some(error) =
            cache_landing_error(&landings, &self.env.inspect_mount_sources(&landing_paths))
        {
            return Err(error);
        }

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

        // Another mount source hort owns rather than finds, made here for the
        // same reason the worktree is: a bind whose source is missing takes the
        // whole container down, and nothing else on the machine creates these.
        self.cache.ensure(&caches.iter().map(|cache| cache.source.clone()).collect::<Vec<_>>())?;
        mounts.extend(caches);

        // Carried in only when some agent announces its completions: the bind is
        // writable, and one nothing ever writes to is a writable path handed to
        // an unrestricted agent for nothing. Its address is the provider's
        // answer and never one worked out here, because whatever later reads the
        // same directory has to arrive at the same path.
        let channel =
            channel_is_declared(self.config).then(|| self.notify.ensure(&name)).transpose()?;
        if let Some(channel) = &channel {
            mounts.push(channel_mount(channel));
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
                    branch_to_checkout,
                    worktree_path.clone(),
                    overlay_path.clone(),
                    timestamp.clone(),
                    timestamp,
                    sink.clone(),
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
            drop_ins: stop_hook_drop_ins(self.config),
            resources: limits,
        })?;

        // Past here the anchor is standing, so a failure leaves a container that
        // is alive, reaches nothing and reports itself healthy, which is worse
        // than an orphan. What decides the undo is that state, never which of the
        // remaining steps was the one that broke, and the failure that comes back
        // is the one that caused it rather than anything the undo produced.
        let running = record.with_token(token);
        let network_spec = NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{}/ns/net", token.pid.0)),
            egress,
            db_forwards: self
                .config
                .network
                .iter()
                .map(|database| DbForward { host: database.host.clone(), port: database.port })
                .collect(),
        };
        let built = self.store.put(&running).and_then(|()| self.network.provision(&network_spec));
        if let Err(failure) = built {
            self.rollback(&running);
            return Err(failure);
        }

        // The last step of the build and the only one that degrades. A box whose
        // completions nobody raises still holds every bit of work in it, and it
        // is the one thing here that can be missed and then arranged afterwards
        // by building the sandbox again.
        if let (Some(events_dir), Some(sink)) = (channel, raising) {
            let watching = NotifySpec {
                name: name.clone(),
                events_dir,
                message: render_notification(self.notification_template(), &name),
                sink,
            };
            if let Err(failure) = self.notify.provision(&watching) {
                warnings.push(Warning::new(format!(
                    "no completion of this sandbox will be raised, because its watcher could not be started: {failure}"
                )));
            }
        }

        self.lock.release(&name)?;
        Ok(warnings)
    }

    /// The message a completion of this sandbox is raised with, as the
    /// configuration asks for it, or `None` where it asks for nothing.
    fn notification_template(&self) -> Option<&str> {
        self.config.notifications.as_ref().and_then(|sink| sink.message.as_deref())
    }

    /// Undo a build that failed with its anchor already standing: the mandatory
    /// teardown order truncated at the container, so the helpers stop before the
    /// namespace they are attached to. Every step is attempted whatever the one
    /// before it did, because a helper that will not die is exactly the
    /// arrangement where giving up early leaves the container standing. What it
    /// cannot stop is left to `ls` and `prune`, like any other orphan.
    fn rollback(&self, record: &SandboxRecord) {
        for step in rollback_plan(record) {
            match step {
                TeardownStep::StopWatcher => {
                    let _ = self.notify.teardown(record.name());
                }
                TeardownStep::StopNetwork => {
                    let _ = self.network.teardown(record.name());
                }
                TeardownStep::StopContainer => {
                    let _ = self.runtime.teardown(record.name());
                }
                // The undo stops processes and touches no disk: the worktree can
                // hold work from an earlier run, and the record is what the next
                // run reads to finish or clean this sandbox.
                TeardownStep::RemoveWorktree | TeardownStep::RemoveMetadata => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::SystemTime;

    use crate::domain::config::{
        Agent, Auth, Cache, CacheDir, Egress, Mounts, Network, Notifications, Notify, Resources,
    };
    use crate::domain::model::{AnchorPid, Capabilities, CgroupCaps, LivenessToken, MountNsInode};
    use crate::ports::{MountAccess, SandboxMount};

    use crate::fakes::{
        FakeCacheProvider, FakeCapabilities, FakeNetwork, FakeNotifyProvider, FakeRegistry,
        FakeRuntime, FakeSandboxLock, FakeWorktreeProvider, InMemoryMetadataStore, ScriptedClock,
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
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
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

    /// A configuration whose one agent announces its own completions, which is
    /// the whole of what gives a sandbox a channel.
    fn config_declaring_a_channel() -> ResolvedConfig {
        ResolvedConfig {
            agents: vec![Agent {
                command: "claude".to_string(),
                auth: Auth::default(),
                notify: Some(Notify { stop_hook: true }),
            }],
            ..healthy_config()
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
        notify: &'a FakeNotifyProvider,
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
            notify,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
    fn up_carries_the_channel_of_a_declared_hook_into_the_sandbox() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The source is the path the provider answered with, never one this
        // command worked out for itself: the layout of the channel belongs to
        // that port because the watcher has to find the same directory, and a
        // second derivation agrees with the first right until one of them moves.
        // The directory also has to be there before the container is built, and
        // it can only be in the spec at all if it was asked for first.
        assert_eq!(
            runtime.started_mounts(),
            vec![SandboxMount {
                source: FakeNotifyProvider::channel_of(&SandboxName::new("demo").unwrap()),
                target: PathBuf::from("/run/hort/notify"),
                access: MountAccess::ReadWrite,
            }]
        );
    }

    #[test]
    fn up_carries_no_channel_when_no_agent_declares_one() {
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // Nothing inside the box ever writes to a channel no agent announces
        // into, and the mount is writable: a sandbox that gets one anyway hands
        // an agent running without restrictions a writable path to the host for
        // nothing in return.
        assert!(runtime.started_mounts().is_empty());
    }

    #[test]
    fn up_hands_the_runtime_the_settings_file_a_declared_hook_needs() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // Which file to drop is decided in the pure layer and the writing is the
        // runtime's, so this handover is the whole of the wiring between them: a
        // build that decides right and hands nothing over comes up with an agent
        // that announces nothing, with every test of the decision still green.
        let dropped = runtime.started_drop_ins();
        assert_eq!(dropped.len(), 1);
        assert_eq!(
            dropped[0].path,
            PathBuf::from("/etc/claude-code/managed-settings.d/hort-notify.json")
        );
    }

    #[test]
    fn up_records_the_sink_a_declared_channel_is_raised_on() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The record is what a later run reads to decide whether this sandbox has
        // a channel at all, so a box with one and nothing written down is a box
        // whose listing pays a disk read per line to learn nothing. This
        // configuration names no sink, and the desktop is the one the build ships.
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap().unwrap();
        assert_eq!(persisted.notify_channel(), Some("desktop"));
    }

    #[test]
    fn up_records_no_channel_when_no_agent_declares_one() {
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The absence is the answer a reader acts on: told a sink, it goes
        // looking for a channel that was never mounted and reports whatever the
        // failed read gives it.
        let persisted = store.get(&SandboxName::new("demo").unwrap()).unwrap().unwrap();
        assert_eq!(persisted.notify_channel(), None);
    }

    #[test]
    fn up_points_the_watcher_at_the_channel_the_box_writes_into() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The same directory on both sides of the boundary, and that is the whole
        // mechanism: the hook writes into a path inside a box that cannot reach
        // the host, and the only reason the host ever hears about it is that this
        // is the host end of that same path. Watching anywhere else leaves every
        // other witness here green and the notification never arriving.
        assert_eq!(
            notify.provisioned_channel(),
            Some(FakeNotifyProvider::channel_of(&SandboxName::new("demo").unwrap()))
        );
    }

    #[test]
    fn up_hands_the_watcher_the_message_the_configuration_asks_for() {
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
            notifications: Some(Notifications {
                sink: None,
                message: Some("<name> is done in the box".to_string()),
            }),
            ..config_declaring_a_channel()
        };
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // Rendered here and once, because the sink is handed a finished message
        // and the process that would otherwise fill it in is the one nothing on
        // the machine can watch. The configured template has to reach it, or the
        // user gets the wording the build shipped and no way to change it.
        assert_eq!(notify.provisioned_message(), Some("demo is done in the box".to_string()));
    }

    #[test]
    fn up_hands_the_watcher_the_sink_the_host_probe_found() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let found = PathBuf::from("/found/only/by/the/probe/notify-send");
        let env = FakeCapabilities::new(Capabilities {
            notify_send: Some(found.clone()),
            ..ready_host()
        });
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The watcher runs the program the precondition found, not whatever a
        // search path answers with hours later, in a process that was forked and
        // can no longer ask anything. A path this deliberate is the point: nothing
        // could arrive at it by looking a program up by name.
        assert_eq!(notify.provisioned_sink(), Some(found));
    }

    #[test]
    fn up_starts_no_watcher_when_no_agent_announces_a_completion() {
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // Nothing ever appends to a channel no agent announces into, so a watcher
        // here is a process per sandbox waiting forever on an event that cannot
        // happen, and one more thing every teardown has to get right.
        assert!(notify.provisioned().is_empty());
    }

    #[test]
    fn up_warns_when_the_host_cannot_raise_a_notification() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { notify_send: None, ..ready_host() });
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The user configured a completion signal and this host cannot raise one.
        // Said nowhere, it becomes an agent that finished hours ago and a person
        // who never came back to look.
        assert!(!warnings.is_empty());
    }

    #[test]
    fn up_starts_no_watcher_when_the_host_cannot_raise_a_notification() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(Capabilities { notify_send: None, ..ready_host() });
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // A watcher whose sink cannot run is a process that wakes on every
        // completion to fail, and the warning above already said what will not
        // happen.
        assert!(notify.provisioned().is_empty());
    }

    #[test]
    fn up_warns_when_the_watcher_cannot_be_started() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::failing_provision();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let warnings = command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // A box whose channel is mounted and whose watcher never started looks
        // exactly like one that works until the first completion is missed, so
        // the failure has to be said on the way out.
        assert!(!warnings.is_empty());
    }

    #[test]
    fn up_leaves_the_sandbox_standing_when_the_watcher_cannot_be_started() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::failing_provision();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // This is the one step of a build that degrades. The costs are not
        // symmetric: tearing the box down loses work nobody else has a copy of,
        // and a mute channel loses a desktop popup.
        assert!(result.is_ok());
        assert!(runtime.teardowns().is_empty());
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
    fn up_rejects_a_cache_that_cannot_land_before_starting_the_container() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        // The host has the configuration directory the user carries in, and does
        // not have the one directory inside it the cache is aimed at.
        let env = FakeCapabilities::new(ready_host())
            .with_missing_mount_source("/home/tester/.config/fish");
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // Two declarations that cannot both hold. Let through, they kill the
        // build with a message about the rootfs, which is the one part of the
        // arrangement nobody declared, so the person reading it goes and rebuilds
        // a rootfs that was fine. Refusing up front names both declarations and
        // the two ways out, and it costs nothing because nothing was built yet.
        assert_eq!(
            result,
            Err(HortError::CacheTargetMissing {
                name: "fish".to_string(),
                target: "/home/hort/.config/fish".to_string(),
                source: "/home/tester/.config".to_string(),
            })
        );
        assert!(runtime.started_env().is_empty());
    }

    #[test]
    fn up_creates_no_worktree_when_it_refuses_a_cache() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host())
            .with_missing_mount_source("/home/tester/.config/fish");
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let _ = command.run(SandboxName::new("demo").unwrap(), None);

        // A refusal over what the configuration says has to happen before hort
        // takes anything, and the worktree is the sharpest thing it takes: git
        // registers it, the record that would make it findable is written later,
        // and a directory that exists is one `git worktree prune` keeps. So a
        // check placed a few lines lower leaves a worktree nothing lists and
        // nothing collects.
        assert!(worktrees.creates().is_empty());
    }

    #[test]
    fn up_builds_when_the_read_only_mount_covering_a_cache_is_not_on_the_host() {
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // A dotfile directory the user stopped keeping is dropped with a warning
        // and the box boots without it, so nothing read-only covers that target
        // any more and the cache lands where every other cache does. Deciding
        // this from what the user declared rather than from what hort is going to
        // mount would refuse a build that has nothing wrong with it.
        assert!(result.is_ok());
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project.clone(),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: None,
            current_dir: PathBuf::from("/home/tester/Downloads"),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project,
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: None,
            current_dir: PathBuf::from("/home/tester/Downloads"),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
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
        let notify = FakeNotifyProvider::new();
        let command = UpCommand {
            project_dir: Some(project.clone()),
            current_dir: project.clone(),
            ..up_command(
                &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env,
                &cache, &notify, &config,
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

    #[test]
    fn up_undoes_the_helpers_before_the_container_when_provisioning_fails() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::failing_provision().with_trace(trace.clone());
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // A build that fails with the anchor already up leaves a box that looks
        // healthy and reaches nothing, which is worse than an orphan: the first
        // symptom is the agent's call to its provider. Undoing it obeys the same
        // physics a teardown does, so the helper the provisioning left running
        // goes before the container whose namespace it is attached to.
        assert!(result.is_err());
        assert_eq!(
            *trace.borrow(),
            vec!["network.teardown".to_string(), "runtime.teardown".to_string()]
        );
    }

    #[test]
    fn up_stops_the_watcher_when_it_undoes_a_failed_build() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::failing_provision();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = config_declaring_a_channel();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // The undo is the teardown order truncated, so every step it carries needs
        // an answer here as much as in `down`: this is the third place they are
        // dispatched, and the one where an unanswered step is invisible, because
        // the plan itself has a witness of its own and this arm does not.
        assert!(result.is_err());
        assert_eq!(notify.teardowns(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn up_keeps_the_worktree_when_provisioning_fails() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::failing_provision();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // The undo stops processes and stops there. This directory is the one
        // thing in the sandbox that can hold work nobody else has a copy of, and
        // a retry of the same name reaches it again.
        assert!(result.is_err());
        assert!(worktrees.exists(Path::new("/state/sandboxes/demo/worktree-demo")));
    }

    #[test]
    fn up_keeps_the_record_when_provisioning_fails() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::failing_provision();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // The record is what the next run reads to finish or clean this sandbox,
        // so removing it would leave the worktree tracked by nothing, which is
        // the leak persisting before the container exists to prevent. It keeps
        // naming the anchor too: rewriting it on a path that is already failing
        // would make a container the undo could not stop read as gone.
        assert!(result.is_err());
        let persisted = store
            .get(&SandboxName::new("demo").unwrap())
            .unwrap()
            .expect("the record survives a failed build");
        assert_eq!(persisted.liveness_token(), Some(canned_token()));
    }

    #[test]
    fn up_reports_the_provisioning_failure_when_the_undo_also_fails() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::failing_provision().with_failing_teardown();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // What comes back names the cause. The scripted provisioning failure and
        // the scripted helper stop fail with different variants on purpose, so a
        // report that swapped one for the other would send the user reading
        // about a cleanup step instead of about the build that broke.
        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn up_stops_the_container_even_when_the_helper_stop_fails() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::failing_provision().with_failing_teardown();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // A helper that will not die is the one arrangement where giving up
        // early leaves exactly the box this undo exists to kill: alive, and
        // reaching nothing. What the undo cannot stop is left to ls and prune,
        // which is a different thing from not trying the rest.
        assert!(result.is_err());
        assert_eq!(runtime.teardowns(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn up_undoes_the_build_when_the_anchor_cannot_be_recorded() {
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new().with_failing_token_write();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), None);

        // Provisioning is not the only thing that can fail with the anchor
        // already up, and the box left behind is the same one either way: alive,
        // and named by nothing that says it is. What decides whether a build is
        // undone is that the anchor was standing when it broke, never which of
        // the steps after it happened to be the one that broke.
        assert!(result.is_err());
        assert_eq!(runtime.teardowns(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn up_undoes_nothing_when_the_build_succeeds() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let lock = FakeSandboxLock::free();
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(false);
        let registry = FakeRegistry::new(vec![]);
        let worktrees = FakeWorktreeProvider::new();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let clock = ScriptedClock::new(SystemTime::UNIX_EPOCH);
        let env = FakeCapabilities::new(ready_host());
        let config = healthy_config();
        let cache = FakeCacheProvider::new();
        let notify = FakeNotifyProvider::new();
        let command = up_command(
            &lock, &store, &probe, &registry, &worktrees, &runtime, &network, &clock, &env, &cache,
            &notify, &config,
        );

        command.run(SandboxName::new("demo").unwrap(), None).unwrap();

        // The undo has to be reachable only from the failing path. An
        // implementation that tears down on the way out of every build would
        // leave every other witness here green while `up` handed back a sandbox
        // it had just killed.
        assert!(trace.borrow().is_empty());
    }
}
