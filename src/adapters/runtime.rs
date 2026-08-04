//! The container half of a sandbox: `LibcontainerRuntime`, the embedded OCI
//! runtime that starts and stops the anchor, and `NullRuntime`, the honest
//! stand-in the binary still wires while the embedded one is being built.
//!
//! Starting an anchor is a fork, not an unshare in place. Creating a user
//! namespace requires a single-threaded process, and hort's own process has to
//! stay outside the sandbox's network namespace so the host-side network helpers
//! keep working afterwards. So the child unshares the namespaces, the parent
//! writes the id mapping for it (a process cannot map its own user namespace),
//! and the child prepares the merged root and starts the anchor before exiting.
//! The anchor holds the namespaces and the mounts, so they outlive their creator.
//!
//! Joining a session is a fork for the same reason, and it climbs on purpose. The
//! tenant API joins only the namespaces the anchor's spec declared, and that spec
//! declares no network namespace, so a session ends up wherever its caller was
//! unless the caller enters the sandbox's network namespace first.
//!
//! The spec the container is built from is assembled by a pure function over
//! plain data, which is what keeps the interesting decisions (empty capability
//! sets, the id mapping, the namespace set, the resource ceiling) testable
//! without a kernel.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;

use libcontainer::container::Container;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::oci_spec::runtime::{
    Capabilities, Linux, LinuxCapabilities, LinuxCpu, LinuxIdMapping, LinuxIdMappingBuilder,
    LinuxMemory, LinuxNamespace, LinuxNamespaceType, LinuxResources, Mount, Process, Root, Spec,
    get_rootless_mounts,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::default::DefaultExecutor;
use libcontainer::workload::{Executor, ExecutorError, ExecutorValidationError};

use crate::adapters::landlock;
use crate::adapters::namespaces::{enter, owning_user_namespace};
use crate::adapters::streams::open_sandbox_log;
use crate::domain::error::HortError;
use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxName};
use crate::ports::{
    ContainerRegistry, ContainerRuntime, OciSpec, RegistryEntry, ResourceLimits, SessionProbe,
    SessionSpec,
};

const SANDBOXES_DIR: &str = "sandboxes";
const BUNDLE_DIR: &str = "bundle";
const CONFIG_FILE: &str = "config.json";
const UPPER_LAYER: &str = "upper";
const WORK_LAYER: &str = "work";
const MERGED_ROOT: &str = "merged";
const WORKDIR: &str = "/workdir";
const DEV_NULL: &str = "/dev/null";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CPU_PERIOD_USEC: u64 = 100_000;
/// The byte both handshakes send; only its arrival, never its value, carries
/// meaning, and the closed pipe that yields none is the abort signal.
const HANDSHAKE: [u8; 1] = [1];
const PROCESS_FAILED: u8 = 0;
const PROCESS_STARTED: u8 = 1;

/// A `ContainerRuntime` (and the read ports the embedded runtime will also serve)
/// for builds without the in-process container runtime.
pub struct NullRuntime;

impl ContainerRuntime for NullRuntime {
    fn start_anchor(&self, _spec: &OciSpec) -> Result<LivenessToken, HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn join_session(&self, _spec: &SessionSpec) -> Result<u32, HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn teardown(&self, _name: &SandboxName) -> Result<(), HortError> {
        Ok(())
    }
}

impl ContainerRegistry for NullRuntime {
    fn list_live(&self) -> Result<Vec<RegistryEntry>, HortError> {
        Ok(Vec::new())
    }
}

impl SessionProbe for NullRuntime {
    fn session_pids(&self, _name: &SandboxName) -> Result<Vec<u32>, HortError> {
        Ok(Vec::new())
    }
}

/// The `ContainerRuntime` hort runs sandboxes on, embedding the OCI runtime in
/// hort's own process: no daemon, no container binary to shell out to.
pub struct LibcontainerRuntime {
    youki_root: PathBuf,
    state_root: PathBuf,
}

impl LibcontainerRuntime {
    /// Build a runtime keeping container state under `youki_root` (the same
    /// registry the live-anchor enumeration walks) and per-sandbox files under
    /// `state_root`.
    pub fn new(youki_root: PathBuf, state_root: PathBuf) -> Self {
        Self { youki_root, state_root }
    }

    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.state_root.join(SANDBOXES_DIR).join(name.as_str())
    }

    fn bundle_dir(&self, name: &SandboxName) -> PathBuf {
        self.sandbox_dir(name).join(BUNDLE_DIR)
    }

    fn container_dir(&self, name: &SandboxName) -> PathBuf {
        self.youki_root.join(name.as_str())
    }

    /// Everything that happens inside the sandbox's own namespaces, from the
    /// forked child: wait for the id mapping, merge the root, and start the
    /// anchor. The detail of a failure travels back to the parent as text,
    /// because an error value cannot cross a process boundary.
    fn build_sandbox(
        &self,
        spec: &OciSpec,
        streams: AnchorStreams,
        mut ready: PipeWriter,
        mut released: PipeReader,
    ) -> Result<u32, String> {
        unshare_sandbox_namespaces()?;
        ready
            .write_all(&HANDSHAKE)
            .map_err(|err| format!("announcing the sandbox namespaces: {err}"))?;

        let mut signal = [0u8; 1];
        match released.read(&mut signal) {
            Ok(0) => return Err("the id mapping was never installed".to_string()),
            Ok(_) => {}
            Err(err) => return Err(format!("waiting for the id mapping: {err}")),
        }

        detach_mount_propagation()?;
        mount_merged_root(spec)?;
        let bundle = self.bundle_dir(&spec.name);
        write_bundle_config(&bundle, &anchor_spec(spec))?;
        start_container(&spec.name, &bundle, &self.youki_root, streams)
    }

    /// The pid of a running sandbox's anchor, taken from the container state the
    /// runtime keeps. It is the anchor's namespaces a session climbs into, and
    /// naming it here is what spares the caller from carrying it around.
    fn anchor_pid(&self, name: &SandboxName) -> Result<u32, HortError> {
        let container = Container::load(self.container_dir(name)).map_err(|err| {
            runtime_failure(format!(
                "join_session: loading the container state of '{}': {err}",
                name.as_str()
            ))
        })?;
        let pid = container.pid().ok_or_else(|| {
            runtime_failure(format!("join_session: sandbox '{}' has no anchor", name.as_str()))
        })?;
        u32::try_from(pid.as_raw()).map_err(|_| {
            runtime_failure(format!(
                "join_session: the runtime reported {} as the anchor pid",
                pid.as_raw()
            ))
        })
    }
}

impl ContainerRuntime for LibcontainerRuntime {
    fn start_anchor(&self, spec: &OciSpec) -> Result<LivenessToken, HortError> {
        let owner = host_owner(&spec.workdir)?;
        let streams = anchor_streams(&self.sandbox_dir(&spec.name)).map_err(runtime_failure)?;
        let (ready_reader, ready_writer) = channel()?;
        let (released_reader, released_writer) = channel()?;
        let (report_reader, report_writer) = channel()?;

        match unsafe { libc::fork() } {
            -1 => {
                Err(runtime_failure(format!("start_anchor: fork: {}", io::Error::last_os_error())))
            }
            0 => {
                drop(ready_reader);
                drop(released_writer);
                drop(report_reader);
                let outcome = self.build_sandbox(spec, streams, ready_writer, released_reader);
                report_and_exit(report_writer, outcome);
            }
            child => {
                drop(ready_writer);
                drop(released_reader);
                drop(report_writer);
                let mapping = install_id_mapping(child, owner, ready_reader, released_writer);
                let report = read_report(report_reader, "start_anchor");
                reap(child);
                mapping?;
                liveness_token(report?)
            }
        }
    }

    fn join_session(&self, spec: &SessionSpec) -> Result<u32, HortError> {
        let anchor = self.anchor_pid(&spec.name)?;
        // Read here, and not where it is applied: by then the session is inside
        // the sandbox, whose root holds no host state directory to read from.
        let reachable = landlock::recorded_connect_ports(&self.sandbox_dir(&spec.name))
            .map_err(|detail| runtime_failure(format!("join_session: {detail}")))?;
        let (report_reader, report_writer) = channel()?;

        // The climb crosses a user namespace, which only a single-threaded
        // process may do, and hort's own process has to stay on the host anyway.
        match unsafe { libc::fork() } {
            -1 => {
                Err(runtime_failure(format!("join_session: fork: {}", io::Error::last_os_error())))
            }
            0 => {
                drop(report_reader);
                let session = ConfinedSession { connect_ports: reachable };
                report_and_exit(
                    report_writer,
                    open_session(spec, anchor, &self.youki_root, session),
                );
            }
            child => {
                drop(report_writer);
                let report = read_report(report_reader, "join_session");
                reap(child);
                report
            }
        }
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        let container_dir = self.container_dir(name);
        if !container_dir.exists() {
            // Teardown runs against a record, and the kernel is free to have
            // outlived it: a sandbox the runtime never knew, or no longer knows,
            // is already torn down.
            return Ok(());
        }

        let mut container = Container::load(container_dir).map_err(|err| {
            runtime_failure(format!(
                "teardown: loading the container state of '{}': {err}",
                name.as_str()
            ))
        })?;
        container.delete(true).map_err(|err| {
            runtime_failure(format!("teardown: stopping '{}': {err}", name.as_str()))
        })?;
        Ok(())
    }
}

/// The host user a sandbox's writes must land as: the owner of the directory
/// bound at `/workdir`, so what the sandbox writes stays editable on the host.
struct HostOwner {
    uid: u32,
    gid: u32,
}

fn host_owner(workdir: &Path) -> Result<HostOwner, HortError> {
    let metadata = fs::metadata(workdir).map_err(|err| {
        runtime_failure(format!("start_anchor: reading the owner of {}: {err}", workdir.display()))
    })?;
    Ok(HostOwner { uid: metadata.uid(), gid: metadata.gid() })
}

fn channel() -> Result<(PipeReader, PipeWriter), HortError> {
    io::pipe().map_err(|err| runtime_failure(format!("creating a pipe: {err}")))
}

fn unshare_sandbox_namespaces() -> Result<(), String> {
    let unshared =
        unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET) };
    if unshared == -1 {
        return Err(format!("creating the sandbox namespaces: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Map the worktree's host owner to root of the sandbox's user namespace, from
/// the parent, once the child reports the namespaces exist. A process cannot map
/// its own user namespace, so this side of the fork is the only one that can do
/// it, and `setgroups` must be denied before the group map is accepted.
fn install_id_mapping(
    child: libc::pid_t,
    owner: HostOwner,
    mut ready: PipeReader,
    mut released: PipeWriter,
) -> Result<(), HortError> {
    let mut signal = [0u8; 1];
    let announced = ready.read(&mut signal).map_err(|err| {
        runtime_failure(format!("start_anchor: waiting for the sandbox namespaces: {err}"))
    })?;
    if announced == 0 {
        // The child died before it had namespaces to map; its own report says why.
        return Ok(());
    }

    write_child_file(child, "setgroups", "deny")?;
    write_child_file(child, "uid_map", &format!("0 {} 1", owner.uid))?;
    write_child_file(child, "gid_map", &format!("0 {} 1", owner.gid))?;

    released.write_all(&HANDSHAKE).map_err(|err| {
        runtime_failure(format!("start_anchor: releasing the sandbox process: {err}"))
    })
}

fn write_child_file(child: libc::pid_t, file: &str, contents: &str) -> Result<(), HortError> {
    let path = format!("/proc/{child}/{file}");
    fs::write(&path, contents)
        .map_err(|err| runtime_failure(format!("start_anchor: writing {path}: {err}")))
}

/// Keep the sandbox's mounts inside the sandbox. A fresh mount namespace starts
/// out sharing propagation with the one it was cloned from, so without this the
/// merged root would appear on the host too.
fn detach_mount_propagation() -> Result<(), String> {
    let detached = unsafe {
        libc::mount(
            ptr::null(),
            c"/".as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            ptr::null(),
        )
    };
    if detached == -1 {
        return Err(format!("detaching the sandbox mounts: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn mount_merged_root(spec: &OciSpec) -> Result<(), String> {
    let upper = spec.overlay.join(UPPER_LAYER);
    let work = spec.overlay.join(WORK_LAYER);
    let merged = spec.overlay.join(MERGED_ROOT);
    for layer in [&upper, &work, &merged] {
        fs::create_dir_all(layer).map_err(|err| format!("creating {}: {err}", layer.display()))?;
    }

    let target = c_path(&merged)?;
    let layers = c_string(&format!(
        "lowerdir={},upperdir={},workdir={}",
        spec.rootfs.display(),
        upper.display(),
        work.display()
    ))?;
    let mounted = unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            target.as_ptr(),
            c"overlay".as_ptr(),
            0,
            layers.as_ptr().cast(),
        )
    };
    if mounted == -1 {
        return Err(format!(
            "merging {} over {}: {}",
            spec.overlay.display(),
            spec.rootfs.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|err| format!("{} is not a usable path: {err}", path.display()))
}

fn c_string(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|err| format!("{value} is not a usable option: {err}"))
}

fn write_bundle_config(bundle: &Path, spec: &Spec) -> Result<(), String> {
    fs::create_dir_all(bundle).map_err(|err| format!("creating {}: {err}", bundle.display()))?;
    let config = bundle.join(CONFIG_FILE);
    spec.save(&config).map_err(|err| format!("writing {}: {err}", config.display()))
}

/// Build the container from the bundle and start its anchor, returning the pid
/// the anchor runs under on the host. The build only creates the container; the
/// anchor is not running until `start`.
fn start_container(
    name: &SandboxName,
    bundle: &Path,
    youki_root: &Path,
    streams: AnchorStreams,
) -> Result<u32, String> {
    fs::create_dir_all(youki_root)
        .map_err(|err| format!("creating {}: {err}", youki_root.display()))?;

    let mut container = ContainerBuilder::new(name.as_str().to_string(), SyscallType::default())
        .with_root_path(youki_root)
        .map_err(|err| format!("rooting the container state at {}: {err}", youki_root.display()))?
        .with_stdin(streams.input)
        .with_stdout(streams.output)
        .with_stderr(streams.errors)
        .as_init(bundle)
        .with_detach(true)
        .build()
        .map_err(|err| format!("building the container: {err}"))?;
    container.start().map_err(|err| format!("starting the anchor: {err}"))?;

    let pid = container
        .pid()
        .ok_or_else(|| "the runtime started the anchor but reported no pid".to_string())?;
    u32::try_from(pid.as_raw())
        .map_err(|_| format!("the runtime reported {} as the anchor pid", pid.as_raw()))
}

/// The streams the anchor is given: the sandbox's log for both outputs, and
/// nothing to read.
struct AnchorStreams {
    input: File,
    output: File,
    errors: File,
}

/// Open them from hort's own process. An anchor keeps whatever it is started
/// with for as long as the sandbox lives, so one started with hort's streams
/// holds a redirected or piped invocation open forever, and holds the writing
/// end of whatever feeds hort open just as long.
///
/// Opening them here rather than inside the sandbox process is what keeps them
/// readable: a file opened after the mount namespace has been unshared carries
/// that namespace's copy of the mount, and once the container pivots away from
/// it `/proc/<anchor>/fd/1` renders as a path relative to a mount the host
/// cannot reach, naming no file anyone can open.
fn anchor_streams(sandbox_dir: &Path) -> Result<AnchorStreams, String> {
    Ok(AnchorStreams {
        input: File::open(DEV_NULL).map_err(|err| format!("opening {DEV_NULL}: {err}"))?,
        output: open_sandbox_log(sandbox_dir)?,
        errors: open_sandbox_log(sandbox_dir)?,
    })
}

/// Climb into the sandbox and start the session there, from the forked child.
/// Every rung is only reachable with the privilege the one below it grants, so
/// the order is not a preference: the owning user namespace first, then the
/// network namespace it owns, then the container's own user namespace, which is
/// what joining the rest of the sandbox demands.
///
/// The network namespace is the rung nothing supplies on its own. The tenant API
/// joins only the namespaces the anchor's spec declared, and that spec declares
/// no network namespace by design, so a session started without this climb runs
/// on the host network while every other thing about it looks right.
fn open_session(
    spec: &SessionSpec,
    anchor: u32,
    youki_root: &Path,
    session: ConfinedSession,
) -> Result<u32, String> {
    let netns = anchor_namespace(anchor, "net")?;
    let container_user = anchor_namespace(anchor, "user")?;
    let owner = owning_user_namespace(netns.as_fd())?;

    enter(&[owner.as_fd(), netns.as_fd(), container_user.as_fd()])?;
    start_session(spec, youki_root, session)
}

fn anchor_namespace(anchor: u32, namespace: &str) -> Result<File, String> {
    let path = format!("/proc/{anchor}/ns/{namespace}");
    File::open(&path).map_err(|err| format!("opening {path}: {err}"))
}

/// The program a session runs, put under the sandbox's restrictions first.
///
/// The runtime calls this from the session's own process at the last step before
/// the exec, after the namespaces are joined and the root is the sandbox's. That
/// is what the restrictions need: they are inherited by whatever is exec'd next,
/// and applied any earlier they would fall on the code still building the
/// session and name a root that is not the one the session will have.
#[derive(Clone)]
struct ConfinedSession {
    connect_ports: Option<Vec<u16>>,
}

impl Executor for ConfinedSession {
    fn exec(&self, spec: &Spec) -> Result<(), ExecutorError> {
        landlock::restrict_session(self.connect_ports.as_deref()).map_err(ExecutorError::Other)?;
        DefaultExecutor {}.exec(spec)
    }

    fn validate(&self, spec: &Spec) -> Result<(), ExecutorValidationError> {
        DefaultExecutor {}.validate(spec)
    }
}

/// Join one session to the sandbox, returning the host pid it runs under. The
/// tenant is detached because a session outlives the call that opened it; the
/// namespaces it is missing are the ones the climb already entered, which it
/// inherits from this process.
fn start_session(
    spec: &SessionSpec,
    youki_root: &Path,
    session: ConfinedSession,
) -> Result<u32, String> {
    let environment: HashMap<String, String> = spec.env.iter().cloned().collect();
    let pid = ContainerBuilder::new(spec.name.as_str().to_string(), SyscallType::default())
        .with_root_path(youki_root)
        .map_err(|err| format!("rooting the container state at {}: {err}", youki_root.display()))?
        .with_executor(session)
        .as_tenant()
        .with_container_args(spec.command.clone())
        .with_cwd(Some(spec.cwd.clone()))
        .with_env(environment)
        .with_detach(true)
        .build()
        .map_err(|err| format!("joining a session to '{}': {err}", spec.name.as_str()))?;

    u32::try_from(pid.as_raw())
        .map_err(|_| format!("the runtime reported {} as the session pid", pid.as_raw()))
}

/// Hand the outcome to the parent and leave at once, without unwinding and
/// without at-exit handlers: everything they would clean up is still owned by
/// the process on the other side of the fork.
fn report_and_exit(mut report: PipeWriter, outcome: Result<u32, String>) -> ! {
    let (message, code) = match outcome {
        Ok(pid) => {
            let mut message = vec![PROCESS_STARTED];
            message.extend_from_slice(&pid.to_le_bytes());
            (message, 0)
        }
        Err(detail) => {
            let mut message = vec![PROCESS_FAILED];
            message.extend_from_slice(detail.as_bytes());
            (message, 1)
        }
    };
    let _ = report.write_all(&message);
    drop(report);
    unsafe { libc::_exit(code) }
}

fn read_report(mut report: PipeReader, operation: &str) -> Result<u32, HortError> {
    let mut message = Vec::new();
    report.read_to_end(&mut message).map_err(|err| {
        runtime_failure(format!("{operation}: reading the sandbox report: {err}"))
    })?;

    if let [PROCESS_STARTED, pid @ ..] = message.as_slice()
        && let Ok(pid) = <[u8; 4]>::try_from(pid)
    {
        return Ok(u32::from_le_bytes(pid));
    }
    if let [PROCESS_FAILED, detail @ ..] = message.as_slice() {
        return Err(runtime_failure(format!("{operation}: {}", String::from_utf8_lossy(detail))));
    }
    Err(runtime_failure(format!("{operation}: the sandbox process reported no pid")))
}

fn reap(child: libc::pid_t) {
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
}

fn liveness_token(pid: u32) -> Result<LivenessToken, HortError> {
    let mount_namespace = format!("/proc/{pid}/ns/mnt");
    let inode = fs::metadata(&mount_namespace)
        .map_err(|err| runtime_failure(format!("start_anchor: reading {mount_namespace}: {err}")))?
        .ino();
    Ok(LivenessToken { pid: AnchorPid(pid), mnt_ns: MountNsInode(inode) })
}

fn runtime_failure(detail: impl Into<String>) -> HortError {
    HortError::ContainerRuntimeFailed { detail: detail.into() }
}

/// The OCI runtime spec of a sandbox's anchor container, assembled from the
/// sandbox data alone: the id mapping is fixed by how the sandbox is built, not
/// by who owns the worktree.
fn anchor_spec(spec: &OciSpec) -> Spec {
    let mut assembled = Spec::default();
    assembled
        .set_hostname(Some(spec.name.as_str().to_string()))
        .set_root(Some(merged_root(&spec.overlay)))
        .set_mounts(Some(sandbox_mounts(&spec.workdir)))
        .set_process(Some(anchor_process(&spec.env)))
        .set_linux(Some(sandbox_linux(spec.resources.as_ref())));
    assembled
}

fn merged_root(overlay: &Path) -> Root {
    let mut root = Root::default();
    root.set_path(overlay.join(MERGED_ROOT));
    // The sandbox writes anywhere in its root; what makes that safe is that the
    // writes land in a layer discarded on teardown, not a read-only root.
    root.set_readonly(Some(false));
    root
}

/// The base mount set plus the worktree. The rootless set is the one that works
/// here: it binds the host `/sys` instead of mounting a fresh one, which a user
/// namespace that does not own the network namespace may not do, and it drops
/// the terminal group id, which is not mapped inside the sandbox.
fn sandbox_mounts(workdir: &Path) -> Vec<Mount> {
    let mut mounts = get_rootless_mounts();
    let mut worktree = Mount::default();
    worktree.set_destination(PathBuf::from(WORKDIR));
    worktree.set_typ(Some("bind".to_string()));
    worktree.set_source(Some(workdir.to_path_buf()));
    worktree.set_options(Some(vec!["rbind".to_string(), "rw".to_string()]));
    mounts.push(worktree);
    mounts
}

fn anchor_process(env: &[(String, String)]) -> Process {
    let mut process = Process::default();
    process.set_args(Some(vec!["sleep".to_string(), "infinity".to_string()]));
    process.set_env(Some(environment(env)));
    process.set_capabilities(Some(no_capabilities()));
    process
}

fn environment(pairs: &[(String, String)]) -> Vec<String> {
    // The anchor is named without a directory, so the runtime resolves it
    // through PATH and refuses to exec at all when the spec carries none.
    let mut environment = vec![format!("PATH={DEFAULT_PATH}")];
    environment.extend(pairs.iter().map(|(key, value)| format!("{key}={value}")));
    environment
}

/// All five capability sets, explicitly empty. Leaving them out is not the same
/// thing: the runtime then keeps its own default set (kill, net_bind_service,
/// audit_write), the ambient one included.
fn no_capabilities() -> LinuxCapabilities {
    let empty = Capabilities::new();
    let mut capabilities = LinuxCapabilities::default();
    capabilities
        .set_bounding(Some(empty.clone()))
        .set_effective(Some(empty.clone()))
        .set_inheritable(Some(empty.clone()))
        .set_permitted(Some(empty.clone()))
        .set_ambient(Some(empty));
    capabilities
}

fn sandbox_linux(resources: Option<&ResourceLimits>) -> Linux {
    let mut linux = Linux::default();
    linux
        .set_namespaces(Some(sandbox_namespaces()))
        .set_uid_mappings(Some(vec![single_id_mapping()]))
        .set_gid_mappings(Some(vec![single_id_mapping()]))
        .set_resources(resources.map(ceiling));
    linux
}

/// No network namespace: the container inherits the one hort created and owns,
/// in either egress posture. A namespace of its own would be one the sandbox can
/// reconfigure its way out of.
fn sandbox_namespaces() -> Vec<LinuxNamespace> {
    [
        LinuxNamespaceType::User,
        LinuxNamespaceType::Mount,
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
    ]
    .into_iter()
    .map(|typ| {
        let mut namespace = LinuxNamespace::default();
        namespace.set_typ(typ);
        namespace
    })
    .collect()
}

/// The single mapping of the sandbox's user namespace. The worktree's owner is
/// already mapped to 0 in the namespace this container is built from, so from
/// here that owner *is* id 0; naming the host-side number instead would point at
/// an id nothing maps and the container would refuse to start.
fn single_id_mapping() -> LinuxIdMapping {
    LinuxIdMappingBuilder::default()
        .container_id(0u32)
        .host_id(0u32)
        .size(1u32)
        .build()
        .expect("an id mapping with all three fields set has nothing left to reject")
}

fn ceiling(limits: &ResourceLimits) -> LinuxResources {
    let mut resources = LinuxResources::default();
    if let Some(bytes) = limits.memory_bytes {
        let mut memory = LinuxMemory::default();
        memory.set_limit(Some(i64::try_from(bytes).unwrap_or(i64::MAX)));
        resources.set_memory(Some(memory));
    }
    if let Some(cpus) = limits.cpus {
        // Two cores' worth of CPU is a share of the bandwidth of every core, not
        // a pinning to two of them: the controller that pins is often not
        // delegated to the user, which would leave the ceiling unenforced.
        let mut cpu = LinuxCpu::default();
        cpu.set_period(Some(CPU_PERIOD_USEC));
        cpu.set_quota(Some((f64::from(cpus) * CPU_PERIOD_USEC as f64).round() as i64));
        resources.set_cpu(Some(cpu));
    }
    resources
}

#[cfg(test)]
mod tests {
    use super::*;

    use libcontainer::oci_spec::runtime::{
        Capabilities, LinuxIdMappingBuilder, LinuxNamespaceType,
    };

    use crate::ports::ResourceLimits;

    fn sandbox_spec() -> OciSpec {
        OciSpec {
            name: SandboxName::new("demo").unwrap(),
            rootfs: PathBuf::from("/base/rootfs"),
            overlay: PathBuf::from("/state/sandboxes/demo/overlay"),
            workdir: PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            env: vec![
                ("HORT_SANDBOX".to_string(), "demo".to_string()),
                ("HORT_WORKTREE".to_string(), "/state/sandboxes/demo/worktree-demo".to_string()),
            ],
            resources: None,
        }
    }

    #[test]
    fn spec_sets_every_capability_set_empty() {
        let assembled = anchor_spec(&sandbox_spec());

        // An omitted capability set is not an empty one: the runtime then leaves
        // its own default set (kill, net_bind_service, audit_write) in place, the
        // ambient set included, which is a silent loss of the empty-capability
        // guarantee. Only setting all five explicitly zeroes them.
        let process = assembled.process().as_ref().unwrap();
        let capabilities = process.capabilities().as_ref().unwrap();
        assert_eq!(capabilities.bounding(), &Some(Capabilities::new()));
        assert_eq!(capabilities.effective(), &Some(Capabilities::new()));
        assert_eq!(capabilities.inheritable(), &Some(Capabilities::new()));
        assert_eq!(capabilities.permitted(), &Some(Capabilities::new()));
        assert_eq!(capabilities.ambient(), &Some(Capabilities::new()));
    }

    #[test]
    fn spec_maps_the_container_user_to_the_single_mapped_id() {
        let assembled = anchor_spec(&sandbox_spec());

        // The worktree owner is mapped to 0 before the container is built, so
        // from inside the namespace the container is built in, that owner IS
        // uid 0. Naming the host-side owner here instead would point the mapping
        // at an id that is not mapped there, and the container refuses to start.
        let single = LinuxIdMappingBuilder::default()
            .host_id(0u32)
            .container_id(0u32)
            .size(1u32)
            .build()
            .unwrap();
        let linux = assembled.linux().as_ref().unwrap();
        assert_eq!(linux.uid_mappings(), &Some(vec![single]));
        assert_eq!(linux.gid_mappings(), &Some(vec![single]));
    }

    #[test]
    fn spec_carries_a_path_so_the_anchor_can_exec() {
        let assembled = anchor_spec(&sandbox_spec());

        // The anchor is named without a directory, so the runtime resolves it
        // through PATH and refuses to exec at all when the spec carries none.
        let process = assembled.process().as_ref().unwrap();
        let environment = process.env().as_ref().unwrap();
        assert!(environment.iter().any(|pair| pair.starts_with("PATH=")));
    }

    #[test]
    fn spec_declares_no_network_namespace() {
        let assembled = anchor_spec(&sandbox_spec());

        // The container inherits the network namespace hort created and owns, in
        // either egress posture. A namespace of its own would hand the agent the
        // one it can reconfigure.
        let linux = assembled.linux().as_ref().unwrap();
        let declared: Vec<LinuxNamespaceType> =
            linux.namespaces().as_ref().unwrap().iter().map(|namespace| namespace.typ()).collect();
        assert!(!declared.contains(&LinuxNamespaceType::Network));
    }

    #[test]
    fn spec_declares_user_mount_pid_ipc_and_uts_namespaces() {
        let assembled = anchor_spec(&sandbox_spec());

        let linux = assembled.linux().as_ref().unwrap();
        let declared: Vec<LinuxNamespaceType> =
            linux.namespaces().as_ref().unwrap().iter().map(|namespace| namespace.typ()).collect();
        assert!(declared.contains(&LinuxNamespaceType::User));
        assert!(declared.contains(&LinuxNamespaceType::Mount));
        assert!(declared.contains(&LinuxNamespaceType::Pid));
        assert!(declared.contains(&LinuxNamespaceType::Ipc));
        assert!(declared.contains(&LinuxNamespaceType::Uts));
    }

    #[test]
    fn spec_names_the_sandbox_as_the_container_hostname() {
        let assembled = anchor_spec(&sandbox_spec());

        // A spec that leaves the hostname out keeps the runtime's own default,
        // which is the first thing a user reads in the prompt of a session: the
        // name of the tool that built the box instead of the name of the box.
        assert_eq!(assembled.hostname(), &Some("demo".to_string()));
    }

    #[test]
    fn spec_runs_sleep_infinity_as_init() {
        let assembled = anchor_spec(&sandbox_spec());

        let process = assembled.process().as_ref().unwrap();
        assert_eq!(process.args(), &Some(vec!["sleep".to_string(), "infinity".to_string()]));
    }

    #[test]
    fn spec_binds_the_worktree_at_workdir() {
        let assembled = anchor_spec(&sandbox_spec());

        let workdir_mount = assembled
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.destination() == &PathBuf::from("/workdir"))
            .unwrap();
        assert_eq!(
            workdir_mount.source(),
            &Some(PathBuf::from("/state/sandboxes/demo/worktree-demo"))
        );
        assert_eq!(workdir_mount.typ(), &Some("bind".to_string()));
    }

    #[test]
    fn spec_roots_the_container_at_the_merged_overlay() {
        let assembled = anchor_spec(&sandbox_spec());

        let root = assembled.root().as_ref().unwrap();
        assert_eq!(root.path(), &PathBuf::from("/state/sandboxes/demo/overlay/merged"));
    }

    #[test]
    fn spec_leaves_the_merged_root_writable() {
        let assembled = anchor_spec(&sandbox_spec());

        // The agent may write anywhere in the merged root; what makes that safe
        // is that the writes land in a disposable upper layer, not that the root
        // is read-only. A spec root defaults to read-only, so this is a value
        // hort has to set.
        let root = assembled.root().as_ref().unwrap();
        assert_ne!(root.readonly(), Some(true));
    }

    #[test]
    fn spec_exports_the_sandbox_and_worktree_environment() {
        let assembled = anchor_spec(&sandbox_spec());

        let process = assembled.process().as_ref().unwrap();
        let environment = process.env().as_ref().unwrap();
        assert!(environment.contains(&"HORT_SANDBOX=demo".to_string()));
        assert!(
            environment.contains(&"HORT_WORKTREE=/state/sandboxes/demo/worktree-demo".to_string())
        );
    }

    #[test]
    fn spec_maps_cpus_to_cpu_max_bandwidth_quota() {
        let limited = OciSpec {
            resources: Some(ResourceLimits { memory_bytes: None, cpus: Some(2.0) }),
            ..sandbox_spec()
        };

        let assembled = anchor_spec(&limited);

        // Two cores' worth of CPU time is a bandwidth quota over the period, not
        // a pinning to two cores: the cpuset controller is often undelegated,
        // which would make a pinned ceiling silently unenforced.
        let linux = assembled.linux().as_ref().unwrap();
        let cpu = linux.resources().as_ref().unwrap().cpu().as_ref().unwrap();
        assert_eq!(cpu.period(), Some(100_000));
        assert_eq!(cpu.quota(), Some(200_000));
        assert_eq!(cpu.cpus(), &None);
    }

    #[test]
    fn spec_maps_memory_to_the_memory_limit() {
        let limited = OciSpec {
            resources: Some(ResourceLimits { memory_bytes: Some(2_147_483_648), cpus: None }),
            ..sandbox_spec()
        };

        let assembled = anchor_spec(&limited);

        let linux = assembled.linux().as_ref().unwrap();
        let memory = linux.resources().as_ref().unwrap().memory().as_ref().unwrap();
        assert_eq!(memory.limit(), Some(2_147_483_648));
    }

    #[test]
    fn spec_omits_resources_when_none_are_configured() {
        let assembled = anchor_spec(&sandbox_spec());

        let linux = assembled.linux().as_ref().unwrap();
        assert_eq!(linux.resources(), &None);
    }

    #[test]
    fn teardown_is_idempotent_for_an_unknown_sandbox() {
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(
            youki_root.path().to_path_buf(),
            state_root.path().to_path_buf(),
        );

        let result = runtime.teardown(&SandboxName::new("ghost").unwrap());

        assert!(result.is_ok());
    }
}

#[cfg(all(test, feature = "privileged-tests"))]
mod privileged_tests {
    use super::*;

    use std::fs;
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use serial_test::serial;
    use tempfile::TempDir;

    use crate::adapters::environment::HostEnvironmentProbe;
    use crate::adapters::helper::a_declared_port;
    use crate::adapters::pasta::PastaNetworkProvider;
    use crate::adapters::proxy;
    use crate::adapters::streams::sandbox_log_path;
    use crate::domain::egress::{EgressPolicy, HostPattern};
    use crate::domain::model::Domain;
    use crate::ports::{DbForward, EnvironmentProbe, NetworkProvider, NetworkSpec};

    const ANCHOR_DEADLINE: Duration = Duration::from_secs(5);
    /// How long a session is given to exec and dial, and therefore also how long
    /// a connection is waited for before its absence counts as absence.
    const SESSION_DEADLINE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(50);
    /// The file a sandbox records the ports its sessions may connect to in.
    const CONNECT_PORTS_FILE: &str = "connect.ports";
    /// The Landlock ABI that carries the network access rights. Below it the
    /// kernel drops the connect rules and reports success, so a test of them
    /// there would be measuring the kernel rather than hort.
    const CONNECT_RESTRICTION_ABI: u8 = 4;

    /// The prepared rootfs these tests boot, or `None` after reporting what is
    /// missing, so a host without one says why it skipped instead of failing.
    fn prepared_rootfs() -> Option<PathBuf> {
        let Ok(configured) = std::env::var("HORT_TEST_ROOTFS") else {
            eprintln!("skipped: set HORT_TEST_ROOTFS to a prepared rootfs directory to run this");
            return None;
        };
        let rootfs = PathBuf::from(configured);
        if !rootfs.is_dir() {
            eprintln!(
                "skipped: rootfs directory '{}' does not exist, prepare it first",
                rootfs.display()
            );
            return None;
        }
        Some(rootfs)
    }

    fn sandbox_spec(rootfs: PathBuf, state_root: &Path) -> OciSpec {
        let workdir = state_root.join("sandboxes/demo/worktree-demo");
        fs::create_dir_all(&workdir).unwrap();
        OciSpec {
            name: SandboxName::new("demo").unwrap(),
            rootfs,
            overlay: state_root.join("sandboxes/demo/overlay"),
            env: vec![("HORT_SANDBOX".to_string(), "demo".to_string())],
            workdir,
            resources: None,
        }
    }

    fn runtime_under(youki_root: &TempDir, state_root: &TempDir) -> LibcontainerRuntime {
        LibcontainerRuntime::new(youki_root.path().to_path_buf(), state_root.path().to_path_buf())
    }

    /// A session that stays alive long enough to be read from the host, which is
    /// the only way a test can ask where a session ended up.
    fn session_spec(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
        }
    }

    fn sandbox_dir(state_root: &Path) -> PathBuf {
        state_root.join("sandboxes/demo")
    }

    /// An allowlisted sandbox declaring one database on the host's own loopback.
    /// pasta splices that port into the sandbox and hort starts no forwarder of
    /// its own for it, so the port is reachable from inside the sandbox and free
    /// for the test to answer on.
    fn allowlist_network_with_a_database(
        name: &SandboxName,
        anchor: u32,
        port: u16,
    ) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Allowlist(vec![HostPattern::Exact(
                Domain::new("api.anthropic.com").unwrap(),
            )]),
            db_forwards: vec![DbForward { host: "127.0.0.1".to_string(), port }],
        }
    }

    fn open_network(name: &SandboxName, anchor: u32) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Open,
            db_forwards: Vec::new(),
        }
    }

    /// A session that dials one port on the sandbox's loopback and leaves. It
    /// reads from nothing, so what the test process was invoked with is not
    /// consumed by a process inside the sandbox.
    fn dialling_session(name: &SandboxName, port: u16) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("/usr/bin/nc 127.0.0.1 {port} < /dev/null"),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
        }
    }

    /// Whether this kernel can enforce a connect-port restriction at all, saying
    /// why it skipped when it cannot, the way a missing rootfs does.
    fn connect_restriction_enforceable() -> bool {
        match HostEnvironmentProbe.detect().landlock_abi {
            Some(abi) if abi >= CONNECT_RESTRICTION_ABI => true,
            _ => {
                eprintln!(
                    "skipped: this kernel reports no Landlock ABI {CONNECT_RESTRICTION_ABI}, so it cannot restrict which ports a session connects to"
                );
                false
            }
        }
    }

    /// Whether anything from the sandbox reached this listener, waiting for it: a
    /// session execs a moment after it is started, so an answer taken right away
    /// is an answer about nothing.
    fn reached_within_deadline(listener: &TcpListener) -> bool {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + SESSION_DEADLINE;
        while Instant::now() < deadline {
            if listener.accept().is_ok() {
                return true;
            }
            sleep(POLL);
        }
        false
    }

    fn namespace_inode(pid: u32, namespace: &str) -> u64 {
        fs::metadata(format!("/proc/{pid}/ns/{namespace}"))
            .expect("the namespace of a live process")
            .ino()
    }

    /// The anchor execs a moment after the container starts, so anything read off
    /// the running anchor has to wait for it or it races the exec.
    fn wait_for_anchor(pid: u32) {
        let deadline = Instant::now() + ANCHOR_DEADLINE;
        while Instant::now() < deadline {
            let running = fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| cmdline.starts_with(b"sleep"));
            if running {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        panic!("the anchor did not exec within {ANCHOR_DEADLINE:?}");
    }

    /// Point this process's input at `path` and hand back the restore. An anchor
    /// inherits whatever hort was invoked with, so a test whose own input is
    /// already `/dev/null` cannot tell an anchor that was detached from its
    /// caller from one that merely inherited a null caller.
    fn redirect_stdin(path: &Path) -> impl FnOnce() {
        let saved = unsafe { libc::dup(0) };
        assert!(saved != -1, "saving the input of the test process");
        let replacement = File::open(path).expect("the file standing in for hort's input");
        assert!(unsafe { libc::dup2(replacement.as_raw_fd(), 0) } != -1, "redirecting the input");
        move || {
            unsafe { libc::dup2(saved, 0) };
            unsafe { libc::close(saved) };
        }
    }

    /// Whether the anchor is gone, waiting for it: the runtime kills it, and the
    /// kernel reaps it a moment later.
    fn stopped_within_deadline(pid: u32) -> bool {
        let deadline = Instant::now() + ANCHOR_DEADLINE;
        while Instant::now() < deadline {
            if !Path::new(&format!("/proc/{pid}/ns/mnt")).exists() {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_starts_and_reports_its_liveness_token() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        let anchor_mnt_ns = fs::metadata(format!("/proc/{}/ns/mnt", token.pid.0)).unwrap();
        assert_eq!(token.mnt_ns.0, anchor_mnt_ns.ino());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_runs_with_an_empty_capability_set() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        wait_for_anchor(token.pid.0);
        let status = fs::read_to_string(format!("/proc/{}/status", token.pid.0)).unwrap();
        assert!(status.contains("CapInh:\t0000000000000000"));
        assert!(status.contains("CapPrm:\t0000000000000000"));
        assert!(status.contains("CapEff:\t0000000000000000"));
        assert!(status.contains("CapBnd:\t0000000000000000"));
        assert!(status.contains("CapAmb:\t0000000000000000"));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_sees_the_worktree_at_workdir() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        fs::write(spec.workdir.join("from-the-host"), "worktree").unwrap();

        let token = runtime.start_anchor(&spec).unwrap();

        wait_for_anchor(token.pid.0);
        let seen = fs::read_to_string(format!("/proc/{}/root/workdir/from-the-host", token.pid.0))
            .unwrap();
        assert_eq!(seen, "worktree");
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_root_is_the_merged_overlay() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        // A file planted in the sandbox's writable layer is visible at the
        // anchor's root only if that root is the overlay merge of the base rootfs
        // and this layer. It exists in neither the base rootfs nor the host root.
        fs::create_dir_all(spec.overlay.join("upper")).unwrap();
        fs::write(spec.overlay.join("upper/from-the-upper-layer"), "upper").unwrap();

        let token = runtime.start_anchor(&spec).unwrap();

        wait_for_anchor(token.pid.0);
        let seen =
            fs::read_to_string(format!("/proc/{}/root/from-the-upper-layer", token.pid.0)).unwrap();
        assert_eq!(seen, "upper");
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_stops_the_anchor() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        runtime.teardown(&spec.name).unwrap();

        assert!(stopped_within_deadline(token.pid.0));
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_writes_its_streams_to_the_sandbox_log() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        // The anchor outlives the command that started it, so an anchor left
        // holding the streams hort was invoked with holds them for the life of
        // the sandbox: a piped or redirected invocation never reaches EOF and its
        // reader waits on a `sleep infinity` that will never write.
        wait_for_anchor(token.pid.0);
        let log = sandbox_log_path(&state_root.path().join("sandboxes/demo"));
        assert_eq!(fs::read_link(format!("/proc/{}/fd/1", token.pid.0)).unwrap(), log);
        assert_eq!(fs::read_link(format!("/proc/{}/fd/2", token.pid.0)).unwrap(), log);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_anchor_reads_from_nothing_rather_than_from_what_invoked_hort() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let invocation_input = state_root.path().join("invocation-input");
        fs::write(&invocation_input, b"").unwrap();
        let restore_stdin = redirect_stdin(&invocation_input);

        let token = runtime.start_anchor(&spec).unwrap();

        restore_stdin();
        wait_for_anchor(token.pid.0);
        // The anchor never reads, but it outlives the command that started it, so
        // one left holding hort's input keeps that end of a pipe open for the life
        // of the sandbox and whoever writes into hort never learns nobody reads.
        assert_eq!(
            fs::read_link(format!("/proc/{}/fd/0", token.pid.0)).unwrap(),
            PathBuf::from("/dev/null")
        );
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_runs_as_the_host_user_that_owns_the_worktree() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        // What the sandbox writes into /workdir has to land on the host owned by
        // the host user that owns the worktree, or the host ends up with files it
        // cannot edit. That holds only while the anchor itself runs as that user.
        wait_for_anchor(token.pid.0);
        let anchor_user = fs::metadata(format!("/proc/{}", token.pid.0)).unwrap().uid();
        assert_eq!(anchor_user, fs::metadata(&spec.workdir).unwrap().uid());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn session_joins_the_sandbox_mount_namespace() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();

        assert_eq!(namespace_inode(session, "mnt"), namespace_inode(token.pid.0, "mnt"));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn session_runs_in_the_sandbox_network_namespace() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();

        // A session inherits the network namespace of the process that opened
        // it, and the sandbox's spec declares none for the tenant API to find,
        // so a session opened straight from hort lands on the host network. It
        // sees the same worktree, runs the same shell and answers every other
        // question here correctly, while the egress allowlist restricts nothing.
        assert_eq!(namespace_inode(session, "net"), namespace_inode(token.pid.0, "net"));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS), pasta and Landlock ABI 4"]
    #[serial]
    fn a_session_cannot_connect_to_a_port_the_sandbox_did_not_record() {
        let Some(rootfs) = prepared_rootfs() else { return };
        if !connect_restriction_enforceable() {
            return;
        }
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let declared = a_declared_port();
        let database = TcpListener::bind(("127.0.0.1", declared)).unwrap();
        provider
            .provision(&allowlist_network_with_a_database(&spec.name, token.pid.0, declared))
            .unwrap();
        // The sandbox stays exactly as it was wired, with that port spliced into
        // it and answered on the host; only what it recorded changes. So a
        // refusal here can come from nothing but the ruleset, which is the whole
        // point: a port the sandbox cannot reach anyway refuses itself, and a
        // test of that would report this layer as working while it was gone.
        let sandbox_dir = sandbox_dir(state_root.path());
        let proxy = proxy::recorded_port(&sandbox_dir).expect("a proxy port");
        fs::write(sandbox_dir.join(CONNECT_PORTS_FILE), format!("{proxy}\n")).unwrap();

        runtime.join_session(&dialling_session(&spec.name, declared)).unwrap();

        assert!(!reached_within_deadline(&database));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn a_session_reaches_the_declared_database_on_the_sandbox_loopback() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let declared = a_declared_port();
        let database = TcpListener::bind(("127.0.0.1", declared)).unwrap();
        provider
            .provision(&allowlist_network_with_a_database(&spec.name, token.pid.0, declared))
            .unwrap();

        runtime.join_session(&dialling_session(&spec.name, declared)).unwrap();

        // A declared database is one loopback port inside the sandbox, and this
        // is the only place that is asserted from inside. It is also what keeps
        // the refusal above honest: the same command dialling the same port,
        // failing only where the record does not name it.
        assert!(reached_within_deadline(&database));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
    #[serial]
    fn an_open_sandbox_leaves_its_sessions_free_to_connect() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(state_root.path().to_path_buf());
        let port = a_declared_port();
        let listening = TcpListener::bind(("127.0.0.1", port)).unwrap();
        provider.provision(&open_network(&spec.name, token.pid.0)).unwrap();

        runtime.join_session(&dialling_session(&spec.name, port)).unwrap();

        // Open egress is unfiltered by contract, and an open sandbox records no
        // ports at all. Reading that silence as an empty set would lock every
        // open sandbox out of the network it is entitled to, which is the shape
        // a fail-closed default takes when it is applied where nothing failed.
        assert!(reached_within_deadline(&listening));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces"]
    #[serial]
    fn start_anchor_fails_with_container_runtime_failed_for_a_missing_rootfs() {
        let youki_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&youki_root, &state_root);
        let absent = state_root.path().join("no-such-rootfs");
        let spec = sandbox_spec(absent, state_root.path());

        let result = runtime.start_anchor(&spec);

        assert!(matches!(result, Err(HortError::ContainerRuntimeFailed { .. })));
    }
}
