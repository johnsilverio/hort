//! Ports: the narrow traits that isolate effects from decisions.
//!
//! Almost everything hort does touches the kernel, the filesystem, git, or a
//! child process. Each such effect sits behind a narrow trait here so the
//! decision logic that drives it can be tested against an in-memory fake. Every
//! port has exactly one real adapter and one test fake.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::domain::egress::EgressPolicy;
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, Capabilities, LivenessToken, SandboxName, SandboxRecord};

/// Is this recorded anchor still the live one? Alive iff the PID exists **and**
/// its mount-namespace inode matches the token. The inode guards against PID
/// reuse. The real adapter reads `/proc`; tests script the answer.
pub trait LivenessProbe {
    fn is_alive(&self, token: &LivenessToken) -> bool;
}

/// A live anchor enumerated from the container-state registry: the sandbox
/// identity paired with the kernel liveness token its anchor runs under. The
/// cross-source reconciler reads these to spot a live anchor that no on-disk
/// record knows about (a lost record). The port that enumerates them is added
/// with the read-side ports; this is only the plain data it yields.
pub struct RegistryEntry {
    pub id: SandboxName,
    pub token: LivenessToken,
}

/// A worktree as the reconciler needs to see it, identified by its host path: a
/// record is judged inconsistent when its worktree path is absent from the
/// listed worktrees. Deliberately minimal; the listing port and the richer
/// fields (branch, dirty state) arrive with later tasks.
pub struct Worktree {
    pub path: PathBuf,
}

/// The container half of a sandbox: start the idle anchor that keeps the box
/// alive, join a session into its namespaces, tear the container down. This owns
/// the container only, never the host-side helpers (pasta, the proxy, the
/// watcher); those are stopped separately and *before* the container.
pub trait ContainerRuntime {
    /// Start the sandbox's anchor process and return the kernel liveness token it
    /// runs under.
    fn start_anchor(&self, spec: &OciSpec) -> Result<LivenessToken, HortError>;
    /// Join a new session into the running sandbox's namespaces.
    fn join_session(&self, name: &SandboxName) -> Result<(), HortError>;
    /// Stop every session and the anchor, and tear the container down. Releases
    /// the worktree mount, so it runs before the worktree is removed.
    fn teardown(&self, name: &SandboxName) -> Result<(), HortError>;
}

/// The host-side egress wiring for a sandbox. Provisioned *after* the container
/// (it attaches to the netns the container joined) and torn down *before* it.
pub trait NetworkProvider {
    /// Wire up egress: pasta always, plus the SNI proxy in allowlist mode.
    fn provision(&self, spec: &NetworkSpec) -> Result<(), HortError>;
    /// Stop pasta and, if running, the proxy for this sandbox.
    fn teardown(&self, name: &SandboxName) -> Result<(), HortError>;
}

/// The persisted memory of which sandboxes should exist. A missing record reads
/// as `Ok(None)`, `put` upserts by name, and `remove` is idempotent. The store
/// is never the authority on liveness, only on intent.
pub trait MetadataStore {
    /// Write `record`, replacing any record of the same name.
    fn put(&self, record: &SandboxRecord) -> Result<(), HortError>;
    /// Read the record of this name, or `Ok(None)` if there is none.
    fn get(&self, name: &SandboxName) -> Result<Option<SandboxRecord>, HortError>;
    /// Every stored record.
    fn list(&self) -> Result<Vec<SandboxRecord>, HortError>;
    /// Remove the record of this name; removing a missing one is `Ok(())`.
    fn remove(&self, name: &SandboxName) -> Result<(), HortError>;
}

/// The git worktrees that back the sandboxes' `/workdir` mounts. The read side
/// supplies the observations `up` needs to decide what to do about a branch
/// before it touches git.
pub trait WorktreeProvider {
    /// Create a worktree for `name` checked out on `branch`.
    fn create(&self, name: &SandboxName, branch: &BranchName) -> Result<Worktree, HortError>;
    /// Remove this sandbox's worktree.
    fn remove(&self, name: &SandboxName) -> Result<(), HortError>;
    /// Every worktree currently registered.
    fn list(&self) -> Result<Vec<Worktree>, HortError>;
    /// Whether the project directory is a git repository at all.
    fn is_git_repo(&self) -> Result<bool, HortError>;
    /// Whether a branch of this name already exists in the repository.
    fn branch_exists(&self, branch: &BranchName) -> Result<bool, HortError>;
    /// Whether this branch is checked out in any worktree, the main checkout
    /// included.
    fn is_checked_out(&self, branch: &BranchName) -> Result<bool, HortError>;
}

/// Serializes the build of a single sandbox name so two concurrent `up`
/// invocations cannot both clear the duplicate-name check and race to create the
/// same sandbox. The lock is released once the build completes, before any
/// session opens, so a fully built sandbox reports as a duplicate rather than as
/// a build still in progress. The real adapter is an advisory file lock the
/// kernel releases on process death, so a crashed `up` never wedges the name.
pub trait SandboxLock {
    /// Try to take the build lock for `name`; `Ok(false)` means another build
    /// already holds it.
    fn try_acquire(&self, name: &SandboxName) -> Result<bool, HortError>;
    /// Release the build lock for `name`.
    fn release(&self, name: &SandboxName) -> Result<(), HortError>;
}

/// The sink an agent-completion signal is raised on. The message arrives already
/// rendered, so the sink only delivers it.
pub trait Notifier {
    fn notify(&self, message: &str) -> Result<(), HortError>;
}

/// The current wall-clock instant, behind a port so age and idle are computed
/// against a time the test fixes.
pub trait Clock {
    fn now(&self) -> SystemTime;
}

/// Host and kernel capability detection. Read-only and infallible: a capability
/// hort cannot find is recorded absent, never an error.
pub trait EnvironmentProbe {
    fn detect(&self) -> Capabilities;
}

/// Enumerate the live anchors the runtime knows about. The cross-source
/// reconciler compares these against the records to spot a live anchor that no
/// record knows about (a lost record).
pub trait ContainerRegistry {
    fn list_live(&self) -> Result<Vec<RegistryEntry>, HortError>;
}

/// The processes joined to a sandbox. A sandbox's sessions are these minus the
/// anchor; the count drives `ls`, the `down` "sessions still open?" gate, and the
/// idle-active decision. Enumeration is race-tolerant: a PID that vanishes
/// mid-scan reads as absent, never an error.
pub trait SessionProbe {
    fn session_pids(&self, name: &SandboxName) -> Result<Vec<u32>, HortError>;
}

/// The OCI runtime spec `start_anchor` builds the anchor container from. An empty
/// placeholder for now; its fields arrive with the first command that builds a
/// real container.
pub struct OciSpec;

/// Everything `provision` needs to wire a sandbox's egress: the sandbox identity,
/// the host path of the network namespace pasta attaches to, the egress policy
/// (which decides whether the proxy runs), and the declared database forwards.
pub struct NetworkSpec {
    pub name: SandboxName,
    pub netns: PathBuf,
    pub egress: EgressPolicy,
    pub db_forwards: Vec<DbForward>,
}

/// One declared database destination, realized as a single forward to this
/// `host:port` through pasta's host gateway. The config's host-vs-network mode is
/// informational and both are realized identically, so it does not survive here.
pub struct DbForward {
    pub host: String,
    pub port: u16,
}
