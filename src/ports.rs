//! Ports: the narrow traits that isolate effects from decisions.
//!
//! Almost everything hort does touches the kernel, the filesystem, git, or a
//! child process. Each such effect sits behind a narrow trait here so the
//! decision logic that drives it can be tested against an in-memory fake. Every
//! port has exactly one real adapter and one test fake.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::domain::egress::EgressPolicy;
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, Capabilities, LivenessToken, SandboxName, SandboxRecord};
use crate::domain::mounts::MountSourceFacts;
use crate::domain::preconditions::RootfsFacts;

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
    /// Join a new session into the running sandbox's namespaces, returning the
    /// session a caller waits on and, when the spec asked for a terminal, the
    /// pty master the sandbox allocated for it.
    /// The sandbox's network namespace is one of the namespaces the session has
    /// to end up in, and it is the one nothing hands over on its own: a session
    /// is born in the network namespace of the process that opened it, so an
    /// implementation that leaves that to the runtime puts the session on the
    /// host network, where an egress allowlist restricts nothing.
    fn join_session(&self, spec: &SessionSpec) -> Result<Session, HortError>;
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

/// Where a sandbox's egress proxy listens, so a session can be told to send its
/// traffic there.
///
/// The only side that can answer is the one that started the proxy, which is why
/// this sits beside the provisioning port rather than in the record: a port
/// number is precisely what a restart makes meaningless. A sandbox that never had
/// a proxy answers `None`, which is what the open posture always looks like.
pub trait ProxyEndpoint {
    fn proxy_port(&self, name: &SandboxName) -> Option<u16>;
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
    /// The corrupt entries `list` skips, surfaced so `prune` can show and clean
    /// them. A dir with no metadata file at all is half-built, not corrupt, and
    /// stays unlisted.
    fn list_corrupt(&self) -> Result<Vec<CorruptEntry>, HortError>;
}

/// A corrupt metadata directory `list` could not read as a record: its raw
/// directory name (an observation, not a validated value) plus why the record
/// failed to load.
pub struct CorruptEntry {
    pub name: String,
    pub detail: String,
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
    /// Whether a worktree is still on disk at `path`.
    ///
    /// This is what "the worktree is present" means when reconciling, and it is
    /// deliberately not "this repository lists it": a sandbox's worktree lives
    /// under a path hort owns, so a run from another project must not read a
    /// live sandbox as one whose worktree vanished. It takes the path rather
    /// than the name because a record is the authority on where its worktree
    /// is, which in no-git mode is the user's own project folder. An
    /// unanswerable read is absence, never an error.
    fn exists(&self, path: &Path) -> bool;
    /// Whether the project directory is a git repository at all.
    fn is_git_repo(&self) -> Result<bool, HortError>;
    /// Whether a branch of this name already exists in the repository.
    fn branch_exists(&self, branch: &BranchName) -> Result<bool, HortError>;
    /// Whether this branch is checked out in any worktree, the main checkout
    /// included.
    fn is_checked_out(&self, branch: &BranchName) -> Result<bool, HortError>;
    /// Whether this sandbox's worktree has uncommitted changes; untracked files
    /// count. The first behavioral consumer is `prune`.
    fn is_dirty(&self, name: &SandboxName) -> Result<bool, HortError>;
    /// Clear every stale `.git/worktrees` registration whose directory vanished.
    /// `prune` runs it once per non-refused, non-declined run; the per-name
    /// `remove` already clears only its own stale entry.
    fn prune_stale(&self) -> Result<(), HortError>;
}

/// The persistent directories a project's dependency cache lives in. hort owns
/// these: it chose where they sit and keys them by project, so one that is not
/// there yet is this project's first run rather than something the user removed.
/// A mount source hort creates and later collects, like the worktrees.
pub trait CacheProvider {
    /// Create every cache directory in `sources` that is not already there.
    fn ensure(&self, sources: &[PathBuf]) -> Result<(), HortError>;
    /// The stored key of every project that has a cache here. A store that was
    /// never written to reads empty, since a user who has never declared a cache
    /// still runs `prune`.
    fn list(&self) -> Result<Vec<String>, HortError>;
    /// Whether the project a stored cache was keyed by is still on disk.
    ///
    /// It answers with a `Result` rather than a plain `bool` because the caller
    /// gates a deletion on it: "not there" and "could not tell" have to stay
    /// distinguishable, and one value carrying both leaves the gate to resolve
    /// the ambiguity toward removal.
    fn project_exists(&self, project: &Path) -> Result<bool, HortError>;
    /// Remove one project's whole cache directory. Idempotent: a key that is not
    /// there is already in the state asked for.
    fn remove(&self, key: &str) -> Result<(), HortError>;
}

/// The per-sandbox directory an agent's completion hook writes its events into.
///
/// The channel's layout belongs here, and the host path is handed back rather
/// than derived again by the caller: the watcher that reads the same directory
/// needs the same answer, and two derivations of one path drift apart in silence.
/// It is a port of its own rather than a directory the record store makes on the
/// side, because that store's contract is records.
pub trait NotifyProvider {
    /// Create this sandbox's channel directory if it is not there, and answer
    /// with its host path. A bind whose source is missing takes the whole
    /// container down, so this happens before the sandbox is built.
    fn ensure(&self, name: &SandboxName) -> Result<PathBuf, HortError>;
    /// Start the host-side watcher of this sandbox's channel, the process that
    /// turns a completion written inside the box into a notification on the
    /// host. It is the fourth of the host-side helpers a sandbox leaves running,
    /// so it is recognized and stopped the way the others are.
    fn provision(&self, spec: &NotifySpec) -> Result<(), HortError>;
    /// Stop this sandbox's watcher. Idempotent: a sandbox that never had one is
    /// already in the state this asks for, which is what lets the teardown plan
    /// carry the step unconditionally.
    fn teardown(&self, name: &SandboxName) -> Result<(), HortError>;
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

/// The channel an agent's completion hook appends to, as the host-side watcher
/// reads it: append detection and nothing more.
///
/// It never parses the event line. What a completion says about itself has no
/// consumer, because the message the sink raises is a template over the sandbox
/// name, so a parse here would be production nothing reads.
pub trait NotifyWatcher {
    /// Wait until the channel is appended to, answering `true` for an append and
    /// `false` once the channel is gone.
    ///
    /// The channel going away is a real state and not an error: `down` removes
    /// the sandbox's own directory out from under a watcher that has to leave
    /// rather than spin. It borrows mutably because reading events borrows the
    /// buffer they are read into, which the watcher owns.
    fn wait_for_append(&mut self) -> Result<bool, HortError>;
}

/// Asks the user to confirm a destructive action; only `down` and `prune` use it.
/// The message arrives already rendered, like the notifier's, and the prompt
/// wording is not a product guarantee: callers assert that the confirmation
/// happened, never its text. Whether stdin is a TTY is not this port's concern,
/// the CLI detects that and passes a plain bool into the command.
pub trait Confirmer {
    fn confirm(&self, message: &str) -> Result<bool, HortError>;
}

/// The current wall-clock instant, behind a port so age and idle are computed
/// against a time the test fixes.
pub trait Clock {
    fn now(&self) -> SystemTime;
}

/// Host and kernel capability detection, plus the reading of one prepared
/// rootfs. Read-only and infallible: a capability hort cannot find is recorded
/// absent and a path it cannot read yields the conservative fact, never an error.
pub trait EnvironmentProbe {
    fn detect(&self) -> Capabilities;
    /// Read the facts a build decides over from the rootfs at `path`. `shell` is
    /// the session shell the configuration declares, `None` when it declares
    /// none.
    fn inspect_rootfs(&self, path: &Path, shell: Option<&str>) -> RootfsFacts;
    /// Read what the host says about the mount sources the configuration
    /// declared, one fact per path, in the order they were asked about.
    ///
    /// It sits here rather than on a port of its own for the same reason the
    /// rootfs read does: both answer what the host has to say about a path the
    /// configuration named, and both serve the same clients through the same
    /// adapter. A path it cannot read yields the conservative fact, so a source
    /// hort is unsure of degrades instead of taking the container down with it.
    fn inspect_mount_sources(&self, paths: &[PathBuf]) -> Vec<MountSourceFacts>;
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

/// Everything `start_anchor` needs to build a sandbox's anchor container. Plain
/// data: no runtime type leaks in here, the adapter is what turns this into the
/// OCI runtime spec the container is built from.
pub struct OciSpec {
    /// The sandbox identity, which is also the container id.
    pub name: SandboxName,
    /// The prepared base rootfs: the overlay's read-only lower layer.
    pub rootfs: PathBuf,
    /// The per-sandbox directory holding the ephemeral overlay layers and the
    /// merged root the container is rooted at.
    pub overlay: PathBuf,
    /// The host directory bound at `/workdir`: the worktree in git mode, the
    /// project folder in no-git mode.
    pub workdir: PathBuf,
    /// Environment pairs every process in the sandbox sees, `HORT_SANDBOX` and
    /// `HORT_WORKTREE` among them.
    pub env: Vec<(String, String)>,
    /// The host paths the configuration declares, mounted read-only, in the
    /// order they are applied. The order is contract rather than presentation:
    /// a mount whose destination lives inside another is hidden by it unless it
    /// comes after.
    pub mounts: Vec<SandboxMount>,
    /// The files hort writes into the sandbox's own writable layer before the
    /// container starts.
    pub drop_ins: Vec<SandboxFile>,
    /// The per-sandbox resource ceiling; `None` leaves the sandbox uncapped.
    pub resources: Option<ResourceLimits>,
}

/// A file hort puts inside a sandbox: where the box finds it, and what it holds.
///
/// Plain data for the same reason the mounts are: which file a configured agent
/// needs is a decision about that agent, and making it in the code that talks to
/// the kernel would leave it with no fast test. A bind mount cannot do this job,
/// because the directories these land in exist in no prepared rootfs and a bind
/// creates no parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxFile {
    /// Where the box finds it, as an absolute container path.
    pub path: PathBuf,
    pub content: String,
}

/// One host path carried into the sandbox: where it comes from, where the box
/// finds it, and whether the box may write through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub access: MountAccess,
}

/// Whether a carried host path is writable from inside the sandbox. An enum
/// rather than a flag because a name that means one of its own values reads as
/// a fact about the mount everywhere it is not that value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountAccess {
    ReadOnly,
    ReadWrite,
}

/// Everything `join_session` needs to open one session inside a running
/// sandbox. Plain data, like the anchor's spec: which sandbox to enter, what the
/// session runs, where it runs it, and what it sees in its environment.
pub struct SessionSpec {
    /// The sandbox to join, which is also the container id.
    pub name: SandboxName,
    /// The program and arguments the session execs, the login shell among them.
    pub command: Vec<String>,
    /// The in-container directory the session starts in.
    pub cwd: PathBuf,
    /// Environment pairs this session sees on top of what the sandbox exports.
    pub env: Vec<(String, String)>,
    /// Whether the sandbox allocates a pty for this session. The caller decides
    /// it, because whether there is a terminal to relay is a fact about the
    /// process that invoked hort and not about the sandbox.
    pub terminal: bool,
}

/// One session opened inside a running sandbox: the host pid it runs under, and
/// the pty master when the spec asked for a terminal.
///
/// The master is the sandbox's own pty, never hort's terminal, which is what
/// keeps a process inside the box from writing to the terminal the user is
/// sitting at. It is per session, so two concurrent attaches to one sandbox
/// never share it.
///
/// `Debug` only: a session owns an open descriptor, so equality is not something
/// two of them can have, and a caller that needs to compare one compares its pid.
#[derive(Debug)]
pub struct Session {
    pub pid: u32,
    pub pty: Option<OwnedFd>,
}

/// The user's terminal for the length of one session: raw mode on hort's own
/// stdin, both copy directions between it and the session's pty, window-size
/// changes forwarded to what runs inside, and the terminal put back the way it
/// was on the way out.
///
/// It is a port of its own rather than another method on the runtime because the
/// container and the user's terminal are unrelated subsystems, and because
/// `attach` and the session `up` opens need the identical relay.
pub trait SessionTerminal {
    /// Hold the terminal until the session ends and report the wait status the
    /// kernel gives for it. A session that asked for no terminal has nothing to
    /// relay and is only waited on.
    fn relay(&self, session: Session) -> Result<i32, HortError>;
}

/// The per-sandbox resource ceiling. `cpus` is an amount of CPU time (2.0 means
/// two cores' worth), never a pinning to particular cores: the cpuset controller
/// is frequently undelegated, the bandwidth one is not.
pub struct ResourceLimits {
    pub memory_bytes: Option<u64>,
    pub cpus: Option<f32>,
}

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

/// Everything the host-side watcher of one sandbox needs: which sandbox it
/// belongs to, the channel directory it observes, the message it raises, and the
/// program that raises it.
///
/// The whole world of that process travels here because the process is forked and
/// then outlives the call, so anything it is not handed it can never ask for. The
/// sink is a resolved path rather than a program name on purpose: the
/// notification runs the binary the host probe found, instead of whatever the
/// search path answers with later, and it is what gives the probe's answer a
/// reader at all.
pub struct NotifySpec {
    pub name: SandboxName,
    pub events_dir: PathBuf,
    pub message: String,
    pub sink: PathBuf,
}
