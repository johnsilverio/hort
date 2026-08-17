//! In-memory test doubles for the ports, plus the generic store contract the
//! real adapter is held to as well. Compiled only under test.

// Shared test infrastructure: several of these doubles are first exercised by the
// command tasks that come next, so they read as unused until then.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use crate::domain::egress::{EgressPolicy, HostPattern};
use crate::domain::error::HortError;
use crate::domain::model::{
    AnchorPid, BranchName, Capabilities, LivenessToken, MountNsInode, SandboxName, SandboxRecord,
};
use crate::domain::mounts::MountSourceFacts;
use crate::domain::preconditions::{ConfiguredShell, RootfsFacts};
use crate::ports::{
    CacheProvider, Clock, Confirmer, ContainerRegistry, ContainerRuntime, CorruptEntry, DbForward,
    EnvironmentProbe, LivenessProbe, MetadataStore, NetworkProvider, NetworkSpec, Notifier,
    NotifyProvider, OciSpec, ProxyEndpoint, RegistryEntry, ResourceLimits, SandboxFile,
    SandboxLock, SandboxMount, Session, SessionProbe, SessionSpec, Worktree, WorktreeProvider,
};

/// The shared teardown-order witness threaded through the fakes that perform a
/// teardown step. Each fake, when given one of these, appends its pinned label as
/// its step runs, so a `down`/`prune` test can assert the mandatory C5 order from
/// a single recorded sequence (the chartered call-order case of testing.md 7).
type TeardownTrace = Rc<RefCell<Vec<String>>>;

/// The records a sandbox should exist, kept in a map keyed by name. Honors the
/// same contract as the file-backed store: `put` upserts, `get` is `Ok(None)`
/// when absent, `remove` is idempotent.
#[derive(Default)]
pub struct InMemoryMetadataStore {
    records: RefCell<HashMap<SandboxName, SandboxRecord>>,
    corrupt: RefCell<Vec<(String, String)>>,
    token_write_fails: bool,
    trace: Option<TeardownTrace>,
}

impl InMemoryMetadataStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the `store.remove` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Script the write that records a running anchor as failing, leaving the
    /// record that was persisted before the container untouched. It is the other
    /// way a build fails with its container already standing.
    pub fn with_failing_token_write(mut self) -> Self {
        self.token_write_fails = true;
        self
    }

    /// Script a corrupt metadata dir of this name, returned by `list_corrupt`.
    /// Its same-named entry is cleared by `remove`, the observable witness that
    /// pruning a corrupt dir took it off disk.
    pub fn with_corrupt_entry(self, name: &str, detail: &str) -> Self {
        self.corrupt.borrow_mut().push((name.to_owned(), detail.to_owned()));
        self
    }
}

impl MetadataStore for InMemoryMetadataStore {
    fn put(&self, record: &SandboxRecord) -> Result<(), HortError> {
        if self.token_write_fails && record.liveness_token().is_some() {
            // An unasserted stand-in error, like the network's scripted
            // provisioning failure: what a test watches is what `up` undoes.
            return Err(HortError::StateIo {
                detail: "fake store: the token write is scripted to fail".to_string(),
            });
        }
        self.records.borrow_mut().insert(record.name().clone(), record.clone());
        Ok(())
    }

    fn get(&self, name: &SandboxName) -> Result<Option<SandboxRecord>, HortError> {
        Ok(self.records.borrow().get(name).cloned())
    }

    fn list(&self) -> Result<Vec<SandboxRecord>, HortError> {
        Ok(self.records.borrow().values().cloned().collect())
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        self.records.borrow_mut().remove(name);
        self.corrupt.borrow_mut().retain(|(corrupt_name, _)| corrupt_name != name.as_str());
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("store.remove".to_string());
        }
        Ok(())
    }

    fn list_corrupt(&self) -> Result<Vec<CorruptEntry>, HortError> {
        Ok(self
            .corrupt
            .borrow()
            .iter()
            .map(|(name, detail)| CorruptEntry { name: name.clone(), detail: detail.clone() })
            .collect())
    }
}

/// Returns a canned liveness token from `start_anchor` and remembers the
/// sessions it opened and the sandboxes it tore down, starting no process at
/// all. Can also be scripted to fail its `start_anchor`, so a test can witness
/// that `up` persists the metadata record before it ever starts the container.
pub struct FakeRuntime {
    token: LivenessToken,
    start_fails: bool,
    started_env: RefCell<Vec<(String, String)>>,
    started_rootfs: RefCell<PathBuf>,
    started_workdir: RefCell<PathBuf>,
    started_mounts: RefCell<Vec<SandboxMount>>,
    started_drop_ins: RefCell<Vec<SandboxFile>>,
    started_resources: RefCell<Option<ResourceLimits>>,
    sessions: RefCell<Vec<SessionSpec>>,
    teardowns: RefCell<Vec<SandboxName>>,
    trace: Option<TeardownTrace>,
}

impl FakeRuntime {
    /// The host pid every session this fake opens reports running under.
    pub const SESSION_PID: u32 = 4321;

    pub fn new(token: LivenessToken) -> Self {
        Self {
            token,
            start_fails: false,
            started_env: RefCell::new(Vec::new()),
            started_rootfs: RefCell::new(PathBuf::new()),
            started_workdir: RefCell::new(PathBuf::new()),
            started_mounts: RefCell::new(Vec::new()),
            started_drop_ins: RefCell::new(Vec::new()),
            started_resources: RefCell::new(None),
            sessions: RefCell::new(Vec::new()),
            teardowns: RefCell::new(Vec::new()),
            trace: None,
        }
    }

    /// A runtime whose `start_anchor` fails. The error value is an unasserted
    /// stand-in: the catalog carries no dedicated runtime-failure message yet, so
    /// the witness is the half-built persisted record, not the error variant.
    pub fn failing_start(token: LivenessToken) -> Self {
        Self { start_fails: true, ..Self::new(token) }
    }

    /// Record the `runtime.teardown` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// The environment pairs of the spec the last `start_anchor` was handed.
    pub fn started_env(&self) -> Vec<(String, String)> {
        self.started_env.borrow().clone()
    }

    /// The base rootfs of the spec the last `start_anchor` was handed.
    pub fn started_rootfs(&self) -> PathBuf {
        self.started_rootfs.borrow().clone()
    }

    /// The host directory the spec the last `start_anchor` was handed binds at
    /// `/workdir`.
    pub fn started_workdir(&self) -> PathBuf {
        self.started_workdir.borrow().clone()
    }

    /// The host paths the spec the last `start_anchor` was handed carries into
    /// the sandbox, in order.
    pub fn started_mounts(&self) -> Vec<SandboxMount> {
        self.started_mounts.borrow().clone()
    }

    /// The files the spec the last `start_anchor` was handed asks to be written
    /// into the sandbox, in order.
    pub fn started_drop_ins(&self) -> Vec<SandboxFile> {
        self.started_drop_ins.borrow().clone()
    }

    /// The memory ceiling of the spec the last `start_anchor` was handed.
    pub fn started_memory_bytes(&self) -> Option<u64> {
        self.started_resources.borrow().as_ref().and_then(|limits| limits.memory_bytes)
    }

    /// The CPU ceiling of the spec the last `start_anchor` was handed.
    pub fn started_cpus(&self) -> Option<f32> {
        self.started_resources.borrow().as_ref().and_then(|limits| limits.cpus)
    }

    /// The sandbox of every session it was asked to open, in order.
    pub fn joins(&self) -> Vec<SandboxName> {
        self.sessions.borrow().iter().map(|spec| spec.name.clone()).collect()
    }

    /// The program and arguments of the session opened last.
    pub fn session_command(&self) -> Vec<String> {
        self.sessions.borrow().last().map(|spec| spec.command.clone()).unwrap_or_default()
    }

    /// The working directory of the session opened last.
    pub fn session_cwd(&self) -> PathBuf {
        self.sessions.borrow().last().map(|spec| spec.cwd.clone()).unwrap_or_default()
    }

    /// The environment pairs of the session opened last.
    pub fn session_env(&self) -> Vec<(String, String)> {
        self.sessions.borrow().last().map(|spec| spec.env.clone()).unwrap_or_default()
    }

    /// Whether the session opened last asked the sandbox for a terminal.
    pub fn session_terminal(&self) -> bool {
        self.sessions.borrow().last().is_some_and(|spec| spec.terminal)
    }

    pub fn teardowns(&self) -> Vec<SandboxName> {
        self.teardowns.borrow().clone()
    }
}

impl ContainerRuntime for FakeRuntime {
    fn start_anchor(&self, spec: &OciSpec) -> Result<LivenessToken, HortError> {
        self.started_env.borrow_mut().clone_from(&spec.env);
        self.started_rootfs.borrow_mut().clone_from(&spec.rootfs);
        self.started_workdir.borrow_mut().clone_from(&spec.workdir);
        self.started_mounts.borrow_mut().clone_from(&spec.mounts);
        self.started_drop_ins.borrow_mut().clone_from(&spec.drop_ins);
        *self.started_resources.borrow_mut() = spec
            .resources
            .as_ref()
            .map(|limits| ResourceLimits { memory_bytes: limits.memory_bytes, cpus: limits.cpus });
        if self.start_fails {
            return Err(HortError::InvalidConfig {
                detail: "fake runtime: start_anchor scripted to fail".to_string(),
            });
        }
        Ok(self.token)
    }

    fn join_session(&self, spec: &SessionSpec) -> Result<Session, HortError> {
        self.sessions.borrow_mut().push(SessionSpec {
            name: spec.name.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            terminal: spec.terminal,
        });
        // No pty: a fake that handed one back would be handing back something it
        // never allocated, and nothing a fake can produce would prove a relay.
        Ok(Session { pid: Self::SESSION_PID, pty: None })
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        self.teardowns.borrow_mut().push(name.clone());
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("runtime.teardown".to_string());
        }
        Ok(())
    }
}

/// Remembers the spec of every egress it provisioned, and which sandboxes it
/// tore down, spawning nothing.
#[derive(Default)]
pub struct FakeNetwork {
    provisioned: RefCell<Vec<NetworkSpec>>,
    teardowns: RefCell<Vec<SandboxName>>,
    provision_fails: bool,
    teardown_fails: bool,
    trace: Option<TeardownTrace>,
}

impl FakeNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider whose `provision` fails, the state a build reaches with its
    /// container already standing and no networking around it.
    pub fn failing_provision() -> Self {
        Self { provision_fails: true, ..Self::default() }
    }

    /// Script the helper stop as failing too, standing in for a helper that will
    /// not go. It fails with a different variant from the provisioning failure,
    /// so a test can tell which of the two came back.
    pub fn with_failing_teardown(mut self) -> Self {
        self.teardown_fails = true;
        self
    }

    /// Record the `network.teardown` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn provisioned(&self) -> Vec<SandboxName> {
        self.provisioned.borrow().iter().map(|spec| spec.name.clone()).collect()
    }

    /// The egress policy of the spec provisioned last.
    pub fn provisioned_egress(&self) -> Option<EgressPolicy> {
        self.provisioned.borrow().last().map(|spec| copy_egress_policy(&spec.egress))
    }

    /// The `host` and `port` of every database forward of the spec provisioned
    /// last.
    pub fn provisioned_forwards(&self) -> Vec<(String, u16)> {
        self.provisioned
            .borrow()
            .last()
            .map(|spec| {
                spec.db_forwards
                    .iter()
                    .map(|forward| (forward.host.clone(), forward.port))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn teardowns(&self) -> Vec<SandboxName> {
        self.teardowns.borrow().clone()
    }
}

impl NetworkProvider for FakeNetwork {
    fn provision(&self, spec: &NetworkSpec) -> Result<(), HortError> {
        self.provisioned.borrow_mut().push(NetworkSpec {
            name: spec.name.clone(),
            netns: spec.netns.clone(),
            egress: copy_egress_policy(&spec.egress),
            db_forwards: spec
                .db_forwards
                .iter()
                .map(|forward| DbForward { host: forward.host.clone(), port: forward.port })
                .collect(),
        });
        if self.provision_fails {
            // An unasserted stand-in error, like the runtime's scripted start
            // failure: what a test watches here is what `up` undoes.
            return Err(HortError::InvalidConfig {
                detail: "fake network: provision scripted to fail".to_string(),
            });
        }
        Ok(())
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        self.teardowns.borrow_mut().push(name.clone());
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("network.teardown".to_string());
        }
        if self.teardown_fails {
            return Err(HortError::StateIo {
                detail: "fake network: teardown scripted to fail".to_string(),
            });
        }
        Ok(())
    }
}

/// Take an owned copy of a provisioned egress policy. The spec arrives by
/// reference and the policy is not clonable, so the fake rebuilds it variant by
/// variant to hand the test a policy it can question.
fn copy_egress_policy(policy: &EgressPolicy) -> EgressPolicy {
    match policy {
        EgressPolicy::Open => EgressPolicy::Open,
        EgressPolicy::Allowlist(patterns) => {
            EgressPolicy::Allowlist(patterns.iter().map(copy_host_pattern).collect())
        }
    }
}

fn copy_host_pattern(pattern: &HostPattern) -> HostPattern {
    match pattern {
        HostPattern::Exact(domain) => HostPattern::Exact(domain.clone()),
        HostPattern::Suffix(domain) => HostPattern::Suffix(domain.clone()),
    }
}

/// Answers with a scripted proxy port, listening on nothing. A sandbox built
/// without one reports no port, which is what every open-posture sandbox looks
/// like.
pub struct FakeProxyEndpoint {
    port: Option<u16>,
}

impl FakeProxyEndpoint {
    /// A sandbox whose proxy answers on `port`.
    pub fn listening_on(port: u16) -> Self {
        Self { port: Some(port) }
    }

    /// A sandbox that never had a proxy started for it.
    pub fn without_proxy() -> Self {
        Self { port: None }
    }
}

impl ProxyEndpoint for FakeProxyEndpoint {
    fn proxy_port(&self, _name: &SandboxName) -> Option<u16> {
        self.port
    }
}

/// Answers every liveness check with the same scripted verdict.
pub struct ScriptedLivenessProbe {
    alive: bool,
}

impl ScriptedLivenessProbe {
    pub fn new(alive: bool) -> Self {
        Self { alive }
    }
}

impl LivenessProbe for ScriptedLivenessProbe {
    fn is_alive(&self, _token: &LivenessToken) -> bool {
        self.alive
    }
}

/// Keeps every rendered message so a test can read them back after the act.
#[derive(Default)]
pub struct RecordingNotifier {
    messages: RefCell<Vec<String>>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages.borrow().clone()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, message: &str) -> Result<(), HortError> {
        self.messages.borrow_mut().push(message.to_owned());
        Ok(())
    }
}

/// Returns a fixed instant the test sets, so age and idle are deterministic.
pub struct ScriptedClock {
    now: SystemTime,
}

impl ScriptedClock {
    pub fn new(now: SystemTime) -> Self {
        Self { now }
    }
}

impl Clock for ScriptedClock {
    fn now(&self) -> SystemTime {
        self.now
    }
}

/// Reports scripted host capabilities, detecting nothing real, and remembers
/// which rootfs it was asked to inspect and with which session shell.
pub struct FakeCapabilities {
    capabilities: Capabilities,
    rootfs_present: bool,
    rootfs_shell_present: bool,
    mount_sources_present: bool,
    absent_mount_sources: Vec<PathBuf>,
    inspections: RefCell<Vec<(PathBuf, Option<String>)>>,
}

impl FakeCapabilities {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            rootfs_present: true,
            rootfs_shell_present: true,
            mount_sources_present: true,
            absent_mount_sources: Vec::new(),
            inspections: RefCell::new(Vec::new()),
        }
    }

    /// Script the configured rootfs as absent from the host: a directory that is
    /// not there provides nothing, so every fact about it reads false.
    pub fn with_missing_rootfs(mut self) -> Self {
        self.rootfs_present = false;
        self
    }

    /// Script a rootfs that is there but does not carry the shell it was asked
    /// about, which is the ordinary case for a shell a person installed on their
    /// own machine.
    pub fn without_the_shell_asked_about(mut self) -> Self {
        self.rootfs_shell_present = false;
        self
    }

    /// Script every declared mount source as absent from the host.
    pub fn with_missing_mount_sources(mut self) -> Self {
        self.mount_sources_present = false;
        self
    }

    /// Script one path as absent while the rest are there: the host that has a
    /// directory but not something inside it.
    pub fn with_missing_mount_source(mut self, path: &str) -> Self {
        self.absent_mount_sources.push(PathBuf::from(path));
        self
    }

    /// The path and session shell of every `inspect_rootfs` call, in order.
    pub fn inspections(&self) -> Vec<(PathBuf, Option<String>)> {
        self.inspections.borrow().clone()
    }
}

impl EnvironmentProbe for FakeCapabilities {
    fn detect(&self) -> Capabilities {
        self.capabilities.clone()
    }

    /// Facts built from the arguments it was given, every one satisfied unless
    /// the rootfs was scripted missing.
    fn inspect_rootfs(&self, path: &Path, shell: Option<&str>) -> RootfsFacts {
        self.inspections.borrow_mut().push((path.to_path_buf(), shell.map(str::to_owned)));
        RootfsFacts {
            path: path.to_path_buf(),
            exists: self.rootfs_present,
            has_default_shell: self.rootfs_present,
            configured_shell: shell.map(|shell| ConfiguredShell {
                path: shell.to_owned(),
                present: self.rootfs_present && self.rootfs_shell_present,
            }),
            workdir_writable: self.rootfs_present,
        }
    }

    /// One fact per path asked about, every source there unless it was scripted
    /// absent on its own or they all were.
    fn inspect_mount_sources(&self, paths: &[PathBuf]) -> Vec<MountSourceFacts> {
        paths
            .iter()
            .map(|path| MountSourceFacts {
                path: path.clone(),
                exists: self.mount_sources_present && !self.absent_mount_sources.contains(path),
            })
            .collect()
    }
}

/// Tracks the worktrees it created so `list` reflects `create`/`remove` without
/// touching git, records the branch of each `create`, and answers the read-side
/// observations (git or not, which branches exist, which are checked out) from
/// scripted state. `new` is a fresh git repository with no branches or
/// worktrees; the builder methods layer scripted state on top.
pub struct FakeWorktreeProvider {
    paths: RefCell<Vec<PathBuf>>,
    present: RefCell<Vec<PathBuf>>,
    creates: RefCell<Vec<BranchName>>,
    is_git_repo: bool,
    existing_branches: Vec<BranchName>,
    checked_out_branches: Vec<BranchName>,
    dirty_worktrees: Vec<SandboxName>,
    failing_dirty_probes: Vec<SandboxName>,
    prune_stale_calls: RefCell<usize>,
    trace: Option<TeardownTrace>,
}

impl FakeWorktreeProvider {
    pub fn new() -> Self {
        Self {
            paths: RefCell::new(Vec::new()),
            present: RefCell::new(Vec::new()),
            creates: RefCell::new(Vec::new()),
            is_git_repo: true,
            existing_branches: Vec::new(),
            checked_out_branches: Vec::new(),
            dirty_worktrees: Vec::new(),
            failing_dirty_probes: Vec::new(),
            prune_stale_calls: RefCell::new(0),
            trace: None,
        }
    }

    /// Record the `worktrees.remove` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Script the project as not a git repository.
    pub fn no_git(mut self) -> Self {
        self.is_git_repo = false;
        self
    }

    /// Script a branch as already existing in the repository.
    pub fn with_existing_branch(mut self, branch: &str) -> Self {
        self.existing_branches.push(BranchName::new(branch).unwrap());
        self
    }

    /// Script a branch as checked out in some worktree.
    pub fn with_checked_out_branch(mut self, branch: &str) -> Self {
        self.checked_out_branches.push(BranchName::new(branch).unwrap());
        self
    }

    /// Seed the canonical worktree of `name` as already listed, without recording
    /// a `create`, simulating a worktree left by a prior crashed build. A listed
    /// worktree is also on disk: the real provider lists only the directories
    /// that are still there.
    pub fn with_listed_worktree(self, name: &SandboxName) -> Self {
        self.paths.borrow_mut().push(fake_worktree_path(name));
        self.present.borrow_mut().push(fake_worktree_path(name));
        self
    }

    /// Seed the canonical worktree of `name` as on disk but absent from this
    /// repository's list, which is what a live sandbox of another project looks
    /// like from here.
    pub fn with_present_worktree(self, name: &SandboxName) -> Self {
        self.present.borrow_mut().push(fake_worktree_path(name));
        self
    }

    /// Script this sandbox's worktree as dirty: `is_dirty` answers `Ok(true)`.
    pub fn with_dirty_worktree(mut self, name: &SandboxName) -> Self {
        self.dirty_worktrees.push(name.clone());
        self
    }

    /// Script this sandbox's dirty probe as failing: `is_dirty` answers `Err`,
    /// standing in for a worktree git cannot inspect.
    pub fn with_failing_dirty_probe(mut self, name: &SandboxName) -> Self {
        self.failing_dirty_probes.push(name.clone());
        self
    }

    /// The branch of every `create` call, in order.
    pub fn creates(&self) -> Vec<BranchName> {
        self.creates.borrow().clone()
    }

    /// How many times `prune_stale` was called.
    pub fn prune_stale_calls(&self) -> usize {
        *self.prune_stale_calls.borrow()
    }
}

impl Default for FakeWorktreeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl WorktreeProvider for FakeWorktreeProvider {
    fn create(&self, name: &SandboxName, branch: &BranchName) -> Result<Worktree, HortError> {
        let path = fake_worktree_path(name);
        self.creates.borrow_mut().push(branch.clone());
        self.paths.borrow_mut().push(path.clone());
        self.present.borrow_mut().push(path.clone());
        Ok(Worktree { path })
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        let path = fake_worktree_path(name);
        self.paths.borrow_mut().retain(|listed| listed != &path);
        self.present.borrow_mut().retain(|on_disk| on_disk != &path);
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("worktrees.remove".to_string());
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<Worktree>, HortError> {
        Ok(self.paths.borrow().iter().cloned().map(|path| Worktree { path }).collect())
    }

    fn exists(&self, path: &Path) -> bool {
        self.present.borrow().iter().any(|on_disk| on_disk == path)
    }

    fn is_git_repo(&self) -> Result<bool, HortError> {
        Ok(self.is_git_repo)
    }

    fn branch_exists(&self, branch: &BranchName) -> Result<bool, HortError> {
        Ok(self.existing_branches.contains(branch))
    }

    fn is_checked_out(&self, branch: &BranchName) -> Result<bool, HortError> {
        Ok(self.checked_out_branches.contains(branch))
    }

    fn is_dirty(&self, name: &SandboxName) -> Result<bool, HortError> {
        if self.failing_dirty_probes.contains(name) {
            // An unasserted stand-in error: no consumer asserts the variant,
            // only that it is an `Err`.
            return Err(HortError::InvalidConfig {
                detail: "fake worktree: dirty probe scripted to fail".to_string(),
            });
        }
        Ok(self.dirty_worktrees.contains(name))
    }

    fn prune_stale(&self) -> Result<(), HortError> {
        *self.prune_stale_calls.borrow_mut() += 1;
        Ok(())
    }
}

fn fake_worktree_path(name: &SandboxName) -> PathBuf {
    PathBuf::from(format!("/state/sandboxes/{0}/worktree-{0}", name.as_str()))
}

/// Yields a scripted list of live anchors for the cross-source reconciler.
pub struct FakeRegistry {
    entries: Vec<(SandboxName, LivenessToken)>,
}

impl FakeRegistry {
    pub fn new(entries: Vec<(SandboxName, LivenessToken)>) -> Self {
        Self { entries }
    }
}

impl ContainerRegistry for FakeRegistry {
    fn list_live(&self) -> Result<Vec<RegistryEntry>, HortError> {
        Ok(self
            .entries
            .iter()
            .map(|(id, token)| RegistryEntry { id: id.clone(), token: *token })
            .collect())
    }
}

/// Remembers the cache directories it was asked to create, creating nothing, and
/// answers about a scripted set of stored keys and living projects. A project it
/// was told nothing about is gone, which is the state a cache has to be in before
/// anything removes it.
#[derive(Default)]
pub struct FakeCacheProvider {
    ensured: RefCell<Vec<PathBuf>>,
    stored_keys: Vec<String>,
    live_projects: Vec<PathBuf>,
    failing_projects: Vec<PathBuf>,
    removed: RefCell<Vec<String>>,
    trace: Option<TeardownTrace>,
}

impl FakeCacheProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the `cache.remove` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Script a stored cache of this key, which `list` reports.
    pub fn with_stored_key(mut self, key: &str) -> Self {
        self.stored_keys.push(key.to_string());
        self
    }

    /// Script this project as still on disk: `project_exists` answers `Ok(true)`.
    pub fn with_live_project(mut self, project: &Path) -> Self {
        self.live_projects.push(project.to_path_buf());
        self
    }

    /// Script this project's presence read as failing: `project_exists` answers
    /// `Err`, standing in for a path hort could not ask the disk about.
    pub fn with_failing_project_probe(mut self, project: &Path) -> Self {
        self.failing_projects.push(project.to_path_buf());
        self
    }

    /// Every directory it was asked to create, in order.
    pub fn ensured(&self) -> Vec<PathBuf> {
        self.ensured.borrow().clone()
    }

    /// Every key it was asked to remove, in order.
    pub fn removed(&self) -> Vec<String> {
        self.removed.borrow().clone()
    }
}

impl CacheProvider for FakeCacheProvider {
    fn ensure(&self, sources: &[PathBuf]) -> Result<(), HortError> {
        self.ensured.borrow_mut().extend(sources.iter().cloned());
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>, HortError> {
        Ok(self.stored_keys.clone())
    }

    fn project_exists(&self, project: &Path) -> Result<bool, HortError> {
        if self.failing_projects.iter().any(|failing| failing == project) {
            return Err(HortError::StateIo {
                detail: format!("could not read {}", project.display()),
            });
        }
        Ok(self.live_projects.iter().any(|live| live == project))
    }

    fn remove(&self, key: &str) -> Result<(), HortError> {
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("cache.remove".to_string());
        }
        self.removed.borrow_mut().push(key.to_string());
        Ok(())
    }
}

/// Remembers the sandboxes it was asked to make a completion channel for, making
/// nothing, and answers with a path nothing else could have arrived at.
///
/// The address is deliberately unlike the layout the real provider uses. What the
/// channel's layout is belongs to this port, so a caller that derived the path
/// itself would agree with a fake that derived it the same way, and the two
/// copies would drift apart with every test still green.
#[derive(Default)]
pub struct FakeNotifyProvider {
    ensured: RefCell<Vec<SandboxName>>,
}

impl FakeNotifyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// The host path this fake answers with for `name`.
    pub fn channel_of(name: &SandboxName) -> PathBuf {
        PathBuf::from(format!("/channel-only-the-provider-knows/{}", name.as_str()))
    }

    /// Every sandbox it was asked to make a channel for, in order.
    pub fn ensured(&self) -> Vec<SandboxName> {
        self.ensured.borrow().clone()
    }
}

impl NotifyProvider for FakeNotifyProvider {
    fn ensure(&self, name: &SandboxName) -> Result<PathBuf, HortError> {
        self.ensured.borrow_mut().push(name.clone());
        Ok(Self::channel_of(name))
    }
}

/// Reports a scripted process list for a sandbox.
pub struct FakeSessionProbe {
    pids: Vec<u32>,
}

impl FakeSessionProbe {
    pub fn new(pids: Vec<u32>) -> Self {
        Self { pids }
    }
}

impl SessionProbe for FakeSessionProbe {
    fn session_pids(&self, _name: &SandboxName) -> Result<Vec<u32>, HortError> {
        Ok(self.pids.clone())
    }
}

/// A scripted build lock, either free or already held, that records the names it
/// was asked to release.
pub struct FakeSandboxLock {
    held: bool,
    releases: RefCell<Vec<SandboxName>>,
}

impl FakeSandboxLock {
    /// A lock no other build holds: `try_acquire` succeeds.
    pub fn free() -> Self {
        Self { held: false, releases: RefCell::new(Vec::new()) }
    }

    /// A lock another build already holds: `try_acquire` reports it taken.
    pub fn held() -> Self {
        Self { held: true, releases: RefCell::new(Vec::new()) }
    }

    pub fn releases(&self) -> Vec<SandboxName> {
        self.releases.borrow().clone()
    }
}

impl SandboxLock for FakeSandboxLock {
    fn try_acquire(&self, _name: &SandboxName) -> Result<bool, HortError> {
        Ok(!self.held)
    }

    fn release(&self, name: &SandboxName) -> Result<(), HortError> {
        self.releases.borrow_mut().push(name.clone());
        Ok(())
    }
}

/// Answers every confirmation with the same scripted verdict and records the
/// prompts it was asked, so a test can assert that the confirmation happened
/// without pinning its wording.
pub struct FakeConfirmer {
    answer: bool,
    prompts: RefCell<Vec<String>>,
}

impl FakeConfirmer {
    /// A confirmer that always answers yes.
    pub fn yes() -> Self {
        Self { answer: true, prompts: RefCell::new(Vec::new()) }
    }

    /// A confirmer that always answers no.
    pub fn no() -> Self {
        Self { answer: false, prompts: RefCell::new(Vec::new()) }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.borrow().clone()
    }
}

impl Confirmer for FakeConfirmer {
    fn confirm(&self, message: &str) -> Result<bool, HortError> {
        self.prompts.borrow_mut().push(message.to_owned());
        Ok(self.answer)
    }
}

// The shared MetadataStore contract: one generic function per behavior, run
// against the in-memory fake here and against the real adapter later, so the
// fake cannot drift from the store it stands in for.

/// A representative git-mode record for a sandbox of this name.
pub fn sample_record(name: &str) -> SandboxRecord {
    SandboxRecord::new(
        SandboxName::new(name).unwrap(),
        Some(BranchName::new(name).unwrap()),
        PathBuf::from(format!("/state/sandboxes/{name}/worktree-{name}")),
        PathBuf::from(format!("/state/sandboxes/{name}/overlay")),
        "2026-06-11T12:00:00Z".to_string(),
        "2026-06-11T12:00:00Z".to_string(),
        None,
        PathBuf::from(format!("/home/tester/projects/{name}")),
    )
}

pub fn metadata_store_round_trips_record<S: MetadataStore>(store: S) {
    let record = sample_record("demo");

    store.put(&record).unwrap();
    let fetched = store.get(&SandboxName::new("demo").unwrap()).unwrap();

    assert_eq!(fetched, Some(record));
}

pub fn metadata_store_returns_none_for_missing_name<S: MetadataStore>(store: S) {
    let fetched = store.get(&SandboxName::new("absent").unwrap()).unwrap();

    assert_eq!(fetched, None);
}

pub fn metadata_store_put_overwrites_existing_record<S: MetadataStore>(store: S) {
    let token = LivenessToken { pid: AnchorPid(4321), mnt_ns: MountNsInode(8765) };
    let updated = sample_record("demo").with_token(token);

    store.put(&sample_record("demo")).unwrap();
    store.put(&updated).unwrap();
    let fetched = store.get(&SandboxName::new("demo").unwrap()).unwrap();

    assert_eq!(fetched, Some(updated));
}

pub fn metadata_store_lists_all_put_records<S: MetadataStore>(store: S) {
    store.put(&sample_record("alpha")).unwrap();
    store.put(&sample_record("beta")).unwrap();

    let listed = store.list().unwrap();
    let mut names: Vec<&str> = listed.iter().map(|record| record.name().as_str()).collect();
    names.sort_unstable();

    assert_eq!(names, ["alpha", "beta"]);
}

pub fn metadata_store_remove_makes_record_missing<S: MetadataStore>(store: S) {
    let name = SandboxName::new("demo").unwrap();
    store.put(&sample_record("demo")).unwrap();

    store.remove(&name).unwrap();

    assert_eq!(store.get(&name).unwrap(), None);
}

pub fn metadata_store_remove_is_idempotent_for_missing_name<S: MetadataStore>(store: S) {
    let name = SandboxName::new("absent").unwrap();

    assert!(store.remove(&name).is_ok());
}

#[test]
fn in_memory_store_round_trips_record() {
    metadata_store_round_trips_record(InMemoryMetadataStore::new());
}

#[test]
fn in_memory_store_returns_none_for_missing_name() {
    metadata_store_returns_none_for_missing_name(InMemoryMetadataStore::new());
}

#[test]
fn in_memory_store_put_overwrites_existing_record() {
    metadata_store_put_overwrites_existing_record(InMemoryMetadataStore::new());
}

#[test]
fn in_memory_store_lists_all_put_records() {
    metadata_store_lists_all_put_records(InMemoryMetadataStore::new());
}

#[test]
fn in_memory_store_remove_makes_record_missing() {
    metadata_store_remove_makes_record_missing(InMemoryMetadataStore::new());
}

#[test]
fn in_memory_store_remove_is_idempotent_for_missing_name() {
    metadata_store_remove_is_idempotent_for_missing_name(InMemoryMetadataStore::new());
}
