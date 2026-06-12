//! In-memory test doubles for the ports, plus the generic store contract the
//! real adapter is held to as well. Compiled only under test.

// Shared test infrastructure: several of these doubles are first exercised by the
// command tasks that come next, so they read as unused until then.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use crate::domain::error::HortError;
use crate::domain::model::{
    AnchorPid, BranchName, Capabilities, LivenessToken, MountNsInode, SandboxName, SandboxRecord,
};
use crate::ports::{
    Clock, Confirmer, ContainerRegistry, ContainerRuntime, EnvironmentProbe, LivenessProbe,
    MetadataStore, NetworkProvider, NetworkSpec, Notifier, OciSpec, RegistryEntry, SandboxLock,
    SessionProbe, Worktree, WorktreeProvider,
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
}

impl MetadataStore for InMemoryMetadataStore {
    fn put(&self, record: &SandboxRecord) -> Result<(), HortError> {
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
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("store.remove".to_string());
        }
        Ok(())
    }
}

/// Returns a canned liveness token from `start_anchor` and remembers which
/// sandboxes it joined and tore down. Can also be scripted to fail its
/// `start_anchor`, so a test can witness that `up` persists the metadata record
/// before it ever starts the container.
pub struct FakeRuntime {
    token: LivenessToken,
    start_fails: bool,
    joins: RefCell<Vec<SandboxName>>,
    teardowns: RefCell<Vec<SandboxName>>,
    trace: Option<TeardownTrace>,
}

impl FakeRuntime {
    pub fn new(token: LivenessToken) -> Self {
        Self {
            token,
            start_fails: false,
            joins: RefCell::new(Vec::new()),
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

    pub fn joins(&self) -> Vec<SandboxName> {
        self.joins.borrow().clone()
    }

    pub fn teardowns(&self) -> Vec<SandboxName> {
        self.teardowns.borrow().clone()
    }
}

impl ContainerRuntime for FakeRuntime {
    fn start_anchor(&self, _spec: &OciSpec) -> Result<LivenessToken, HortError> {
        if self.start_fails {
            return Err(HortError::InvalidConfig {
                detail: "fake runtime: start_anchor scripted to fail".to_string(),
            });
        }
        Ok(self.token)
    }

    fn join_session(&self, name: &SandboxName) -> Result<(), HortError> {
        self.joins.borrow_mut().push(name.clone());
        Ok(())
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        self.teardowns.borrow_mut().push(name.clone());
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("runtime.teardown".to_string());
        }
        Ok(())
    }
}

/// Remembers which sandboxes it provisioned egress for and tore down, spawning
/// nothing.
#[derive(Default)]
pub struct FakeNetwork {
    provisioned: RefCell<Vec<SandboxName>>,
    teardowns: RefCell<Vec<SandboxName>>,
    trace: Option<TeardownTrace>,
}

impl FakeNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the `network.teardown` step on the shared teardown trace.
    pub fn with_trace(mut self, trace: TeardownTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn provisioned(&self) -> Vec<SandboxName> {
        self.provisioned.borrow().clone()
    }

    pub fn teardowns(&self) -> Vec<SandboxName> {
        self.teardowns.borrow().clone()
    }
}

impl NetworkProvider for FakeNetwork {
    fn provision(&self, spec: &NetworkSpec) -> Result<(), HortError> {
        self.provisioned.borrow_mut().push(spec.name.clone());
        Ok(())
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        self.teardowns.borrow_mut().push(name.clone());
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("network.teardown".to_string());
        }
        Ok(())
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

/// Reports scripted host capabilities, detecting nothing real.
pub struct FakeCapabilities {
    capabilities: Capabilities,
}

impl FakeCapabilities {
    pub fn new(capabilities: Capabilities) -> Self {
        Self { capabilities }
    }
}

impl EnvironmentProbe for FakeCapabilities {
    fn detect(&self) -> Capabilities {
        self.capabilities.clone()
    }
}

/// Tracks the worktrees it created so `list` reflects `create`/`remove` without
/// touching git, records the branch of each `create`, and answers the read-side
/// observations (git or not, which branches exist, which are checked out) from
/// scripted state. `new` is a fresh git repository with no branches or
/// worktrees; the builder methods layer scripted state on top.
pub struct FakeWorktreeProvider {
    paths: RefCell<Vec<PathBuf>>,
    creates: RefCell<Vec<BranchName>>,
    is_git_repo: bool,
    existing_branches: Vec<BranchName>,
    checked_out_branches: Vec<BranchName>,
    trace: Option<TeardownTrace>,
}

impl FakeWorktreeProvider {
    pub fn new() -> Self {
        Self {
            paths: RefCell::new(Vec::new()),
            creates: RefCell::new(Vec::new()),
            is_git_repo: true,
            existing_branches: Vec::new(),
            checked_out_branches: Vec::new(),
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
    /// a `create`, simulating a worktree left by a prior crashed build.
    pub fn with_listed_worktree(self, name: &SandboxName) -> Self {
        self.paths.borrow_mut().push(fake_worktree_path(name));
        self
    }

    /// The branch of every `create` call, in order.
    pub fn creates(&self) -> Vec<BranchName> {
        self.creates.borrow().clone()
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
        Ok(Worktree { path })
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        let path = fake_worktree_path(name);
        self.paths.borrow_mut().retain(|present| present != &path);
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push("worktrees.remove".to_string());
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<Worktree>, HortError> {
        Ok(self.paths.borrow().iter().cloned().map(|path| Worktree { path }).collect())
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

    fn is_dirty(&self, _name: &SandboxName) -> Result<bool, HortError> {
        Ok(false)
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
