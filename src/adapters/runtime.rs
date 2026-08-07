//! The container half of a sandbox: `LibcontainerRuntime`, the embedded OCI
//! runtime that starts and stops the anchor, and `NullRuntime`, the honest
//! stand-in that answers no session while the process list is being built.
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
//! A session that asks for a terminal gets one of the sandbox's own, made from
//! the sandbox's `/dev/pts` and handed back over a socket. Two things about that
//! are not free to move: the socket has to be listening before the fork, because
//! what connects to it is the process building the session, and the status of a
//! session is only readable because this process claims every session it starts
//! before forking away from it.
//!
//! Enumerating the live anchors reads the same state back. Loading a container
//! refreshes its status against the `/proc` entry of the pid it recorded, so
//! walking the container states and loading each entry answers which anchors are
//! up without a daemon and without hort ever parsing the runtime's file format.
//!
//! Everything this adapter writes is meaningless once the machine restarts, so it
//! all lives under the runtime root: the container states under one directory, the
//! files belonging to one sandbox under another. Keeping them apart is what lets a
//! sandbox be named after either directory without one family landing inside the
//! other. What remembers the sandbox across a reboot reaches here through the spec
//! instead, and this adapter never writes to it.
//!
//! The spec the container is built from is assembled by a pure function over
//! plain data, which is what keeps the interesting decisions (empty capability
//! sets, the id mapping, the namespace set, the resource ceiling) testable
//! without a kernel.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::{Container, ContainerStatus};
use libcontainer::oci_spec::runtime::{
    Capabilities, Linux, LinuxCapabilities, LinuxCpu, LinuxIdMapping, LinuxIdMappingBuilder,
    LinuxMemory, LinuxNamespace, LinuxNamespaceType, LinuxResources, Mount, Process, Root, Spec,
    get_rootless_mounts,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::default::DefaultExecutor;
use libcontainer::workload::{Executor, ExecutorError, ExecutorValidationError};

use crate::adapters::console;
use crate::adapters::landlock;
use crate::adapters::namespaces::{enter, owning_user_namespace};
use crate::adapters::streams::{open_sandbox_log, sandbox_log_path};
use crate::domain::error::HortError;
use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxName};
use crate::domain::mounts::SANDBOX_HOME;
use crate::ports::{
    ContainerRegistry, ContainerRuntime, OciSpec, RegistryEntry, ResourceLimits, SandboxMount,
    Session, SessionProbe, SessionSpec,
};

const SANDBOXES_DIR: &str = "sandboxes";
const CONTAINERS_DIR: &str = "containers";
const BUNDLE_DIR: &str = "bundle";
const CONSOLE_SUFFIXES: [&str; 2] = [".console", ".process.json"];
const CONFIG_FILE: &str = "config.json";
const UPPER_LAYER: &str = "upper";
const WORK_LAYER: &str = "work";
const MERGED_ROOT: &str = "merged";
const WORKDIR: &str = "/workdir";
const SANDBOX_TMP: &str = "/tmp";
const DEV_NULL: &str = "/dev/null";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CPU_PERIOD_USEC: u64 = 100_000;
/// The byte both handshakes send; only its arrival, never its value, carries
/// meaning, and the closed pipe that yields none is the abort signal.
const HANDSHAKE: [u8; 1] = [1];
const PROCESS_FAILED: u8 = 0;
const PROCESS_STARTED: u8 = 1;

/// A `SessionProbe` for builds without the per-sandbox process list.
pub struct NullRuntime;

impl SessionProbe for NullRuntime {
    fn session_pids(&self, _name: &SandboxName) -> Result<Vec<u32>, HortError> {
        Ok(Vec::new())
    }
}

/// The `ContainerRuntime` hort runs sandboxes on, embedding the OCI runtime in
/// hort's own process: no daemon, no container binary to shell out to.
pub struct LibcontainerRuntime {
    runtime_root: PathBuf,
}

impl LibcontainerRuntime {
    /// Build a runtime keeping the container states and each sandbox's own
    /// runtime files under `runtime_root`, which is also the registry the
    /// live-anchor enumeration walks.
    pub fn new(runtime_root: PathBuf) -> Self {
        Self { runtime_root }
    }

    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.runtime_root.join(SANDBOXES_DIR).join(name.as_str())
    }

    fn bundle_dir(&self, name: &SandboxName) -> PathBuf {
        self.sandbox_dir(name).join(BUNDLE_DIR)
    }

    /// Where every container state lives. A directory of its own, because a
    /// sandbox may be named after the directory its neighbours keep their runtime
    /// files in, and one family nested in the other would put that sandbox's state
    /// on top of all of theirs.
    fn containers_root(&self) -> PathBuf {
        self.runtime_root.join(CONTAINERS_DIR)
    }

    fn container_dir(&self, name: &SandboxName) -> PathBuf {
        self.containers_root().join(name.as_str())
    }

    /// Take back what this adapter wrote for one sandbox, and the directory it
    /// wrote it in once nothing is left there.
    ///
    /// Only its own files: the pid files next to them belong to the host-side
    /// helpers, whose own teardown is the only thing that stops the processes they
    /// name, and a sweep of the whole directory would leave a survivor running
    /// with nothing left to recognize it by. Whichever side empties the directory
    /// last removes it, and one that will not go is a directory the next restart
    /// takes away.
    fn remove_runtime_files(&self, name: &SandboxName) {
        let sandbox_dir = self.sandbox_dir(name);
        let _ = fs::remove_file(sandbox_log_path(&sandbox_dir));
        let _ = fs::remove_dir_all(self.bundle_dir(name));
        remove_console_leftovers(&sandbox_dir);
        let _ = fs::remove_dir(&sandbox_dir);
    }

    /// Stop the sandbox's container, and with it every process joined to it.
    fn stop_container(&self, name: &SandboxName) -> Result<(), HortError> {
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
        // Not redundant: delete races systemd's collection of the emptied scope, so
        // a late StopUnit hits "Unit not loaded" and aborts before the container dir
        // is removed. That first failure is the proof the retry needs to finish.
        if container.delete(true).is_err() {
            container.delete(true).map_err(|err| {
                runtime_failure(format!("teardown: stopping '{}': {err}", name.as_str()))
            })?;
        }
        Ok(())
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
        start_container(&spec.name, &bundle, &self.containers_root(), streams)
    }

    /// The console this session asks for, or `None` when it asks for no terminal
    /// and runs on the streams it inherits instead.
    fn console_for(&self, spec: &SessionSpec) -> Result<Option<SessionConsole>, HortError> {
        if !spec.terminal {
            return Ok(None);
        }
        SessionConsole::open(&self.sandbox_dir(&spec.name), spec)
            .map(Some)
            .map_err(|detail| runtime_failure(format!("join_session: {detail}")))
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
        let sandbox_dir = self.sandbox_dir(&spec.name);
        fs::create_dir_all(&sandbox_dir).map_err(|err| {
            runtime_failure(format!("start_anchor: creating {}: {err}", sandbox_dir.display()))
        })?;
        let streams = anchor_streams(&sandbox_dir).map_err(runtime_failure)?;
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
                let anchor = report?;
                liveness_token(anchor).map_err(|err| {
                    runtime_failure(format!("start_anchor: reading /proc/{anchor}/ns/mnt: {err}"))
                })
            }
        }
    }

    fn join_session(&self, spec: &SessionSpec) -> Result<Session, HortError> {
        let anchor = self.anchor_pid(&spec.name)?;
        // Read here, and not where it is applied: by then the session is inside
        // the sandbox, whose root holds nothing of the host to read it from.
        let reachable = landlock::recorded_connect_ports(&self.sandbox_dir(&spec.name))
            .map_err(|detail| runtime_failure(format!("join_session: {detail}")))?;
        let console = self.console_for(spec)?;
        let (report_reader, report_writer) = channel()?;
        become_subreaper()?;

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
                    open_session(spec, anchor, &self.containers_root(), session, console.as_ref()),
                );
            }
            child => {
                drop(report_writer);
                // Taken while the child is still building, which is when the
                // terminal is sent. However that goes, the report is still read
                // and the child still reaped afterwards, so a console that failed
                // leaves behind no session nobody is going to hear about.
                let master = match &console {
                    Some(console) => console.accept_master(&report_reader),
                    None => Ok(None),
                };
                let report = read_report(report_reader, "join_session");
                reap(child);

                let pid = report?;
                let pty =
                    master.map_err(|detail| runtime_failure(format!("join_session: {detail}")))?;
                if console.is_some() && pty.is_none() {
                    return Err(runtime_failure(
                        "join_session: the session started without the terminal it asked for",
                    ));
                }
                Ok(Session { pid, pty })
            }
        }
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        // The files go after the container and not before it, because the anchor
        // is the last process writing to the log.
        self.stop_container(name)?;
        self.remove_runtime_files(name);
        Ok(())
    }
}

impl ContainerRegistry for LibcontainerRuntime {
    fn list_live(&self) -> Result<Vec<RegistryEntry>, HortError> {
        let containers_root = self.containers_root();
        let container_dirs = match fs::read_dir(&containers_root) {
            Ok(entries) => entries,
            // Nothing has been built on this boot, so no anchor is alive. Every
            // command that reconciles asks this before the first sandbox exists.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            // A root that cannot be read is not an empty root: answering "no
            // anchor is alive" without knowing would hand every running sandbox
            // to `prune` as debris.
            Err(err) => {
                return Err(runtime_failure(format!(
                    "list_live: reading {}: {err}",
                    containers_root.display()
                )));
            }
        };

        Ok(container_dirs.flatten().filter_map(|entry| live_anchor(&entry.path())).collect())
    }
}

/// The live anchor a container directory describes, or `None` when it describes
/// none.
///
/// Every way of failing to make sense of one directory yields that same `None`.
/// The state is written by another process and the kernel is free to have
/// outlived it, so a half-written build or a container removed mid-walk is an
/// ordinary finding here; erroring on one would take down every command that
/// reconciles, leaving no way out but deleting files by hand.
fn live_anchor(container_dir: &Path) -> Option<RegistryEntry> {
    let container = Container::load(container_dir.to_path_buf()).ok()?;
    if container.status() != ContainerStatus::Running {
        return None;
    }
    let id = SandboxName::new(container.id()).ok()?;
    let pid = u32::try_from(container.pid()?.as_raw()).ok()?;
    Some(RegistryEntry { id, token: liveness_token(pid).ok()? })
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
    containers_root: &Path,
    streams: AnchorStreams,
) -> Result<u32, String> {
    fs::create_dir_all(containers_root)
        .map_err(|err| format!("creating {}: {err}", containers_root.display()))?;

    let mut container = ContainerBuilder::new(name.as_str().to_string(), SyscallType::default())
        .with_root_path(containers_root)
        .map_err(|err| {
            format!("rooting the container state at {}: {err}", containers_root.display())
        })?
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

/// What one session's terminal takes on the host: the file the tenant reads its
/// terminal request from, and the socket the sandbox sends the pty master back
/// on.
///
/// Both belong to a single session and are removed once it has been opened. Two
/// attaches to the same sandbox at the same time are ordinary, and neither may
/// find the other's socket.
struct SessionConsole {
    process_file: PathBuf,
    socket_path: PathBuf,
    listener: UnixListener,
}

impl SessionConsole {
    fn open(sandbox_dir: &Path, spec: &SessionSpec) -> Result<Self, String> {
        fs::create_dir_all(sandbox_dir)
            .map_err(|err| format!("creating {}: {err}", sandbox_dir.display()))?;
        let session = format!("session-{}-{}", std::process::id(), next_session_number());
        let process_file = sandbox_dir.join(format!("{session}.process.json"));
        let process = serde_json::to_vec(&session_process(spec))
            .map_err(|err| format!("describing the session process: {err}"))?;
        fs::write(&process_file, process)
            .map_err(|err| format!("writing {}: {err}", process_file.display()))?;

        // Bound before the fork, because the sandbox connects from inside the
        // build and finds nothing to connect to if this is left until after it.
        let socket_name = format!("{session}.console");
        let listener = listen_in(sandbox_dir, &socket_name)?;
        Ok(Self { process_file, socket_path: sandbox_dir.join(socket_name), listener })
    }

    /// The pty master the sandbox sends, waited for while the session is being
    /// built. `None` means the build reported before it ever connected: the
    /// report the caller reads next says why, and waiting for a master that will
    /// never arrive hangs instead of reporting it.
    fn accept_master(&self, report: &PipeReader) -> Result<Option<OwnedFd>, String> {
        if !readable_before_report(self.listener.as_raw_fd(), report)? {
            return Ok(None);
        }
        let (stream, _) = self
            .listener
            .accept()
            .map_err(|err| format!("accepting the console connection: {err}"))?;
        if !readable_before_report(stream.as_raw_fd(), report)? {
            return Ok(None);
        }
        console::receive_descriptor(&stream).map(Some)
    }
}

impl Drop for SessionConsole {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.process_file);
    }
}

/// Take away the console files of sessions that were never cleaned up after. A
/// session removes its own the moment it has been opened, so what this finds is
/// what a killed hort left behind.
fn remove_console_leftovers(sandbox_dir: &Path) {
    let Ok(entries) = fs::read_dir(sandbox_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if CONSOLE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Listen on a socket named `name` inside `dir`, at an address whose size does
/// not depend on what `dir` costs. The socket still lands on the real path,
/// which is what the sandbox connects to and what removes it afterwards.
///
/// A socket address holds 107 bytes of path where a path holds 4096, so naming
/// the directory in the address spends the budget on the depth of the runtime
/// root and leaves the rest to the sandbox name: a root the user pointed
/// somewhere of their own can take the lot. Naming the directory by an open
/// descriptor instead costs the same few bytes however deep the sandbox lives.
fn listen_in(dir: &Path, name: &str) -> Result<UnixListener, String> {
    let directory = File::open(dir).map_err(|err| format!("opening {}: {err}", dir.display()))?;
    let address = format!("/proc/self/fd/{}/{name}", directory.as_raw_fd());
    UnixListener::bind(address)
        .map_err(|err| format!("listening on {}: {err}", dir.join(name).display()))
}

/// Whether `wanted` has something to take, waiting for it, and `false` once the
/// session has reported instead. The report is written after the terminal is
/// sent, so a report arriving first is a session that will send none.
fn readable_before_report(wanted: RawFd, report: &PipeReader) -> Result<bool, String> {
    let mut watched = [
        libc::pollfd { fd: wanted, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: report.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        if unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, -1) } == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("waiting for the session terminal: {err}"));
        }
        if watched[0].revents != 0 {
            return Ok(true);
        }
        if watched[1].revents != 0 {
            return Ok(false);
        }
    }
}

fn next_session_number() -> u64 {
    static OPENED: AtomicU64 = AtomicU64::new(0);
    OPENED.fetch_add(1, Ordering::Relaxed)
}

/// Make hort the parent a session falls to when the process that started it
/// exits. The session is built by a forked child that reports the pid and dies,
/// so without this the kernel hands the session to init, and a process nobody is
/// the parent of is a process nobody can learn the exit status of.
fn become_subreaper() -> Result<(), HortError> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong) } == -1 {
        return Err(runtime_failure(format!(
            "join_session: claiming the sessions this process starts: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// The process a session with a terminal runs. The tenant API takes a terminal
/// request only through a process file: everything it builds from its own
/// setters asks for none.
fn session_process(spec: &SessionSpec) -> Process {
    let mut process = Process::default();
    process.set_args(Some(spec.command.clone()));
    process.set_cwd(spec.cwd.clone());
    process.set_env(Some(environment(&spec.env)));
    process.set_terminal(Some(true));
    // Set here because a process file replaces what the tenant API would have
    // built, and what it would have built inherits the sandbox's empty set.
    process.set_capabilities(Some(no_capabilities()));
    process
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
    containers_root: &Path,
    session: ConfinedSession,
    console: Option<&SessionConsole>,
) -> Result<u32, String> {
    let netns = anchor_namespace(anchor, "net")?;
    let container_user = anchor_namespace(anchor, "user")?;
    let owner = owning_user_namespace(netns.as_fd())?;

    enter(&[owner.as_fd(), netns.as_fd(), container_user.as_fd()])?;
    start_session(spec, containers_root, session, console)
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
///
/// A session that asked for a terminal is described by a file instead of by the
/// builder's own setters, which is the only way the tenant API takes the
/// request, and the runtime insists on a console socket for exactly that pairing.
fn start_session(
    spec: &SessionSpec,
    containers_root: &Path,
    session: ConfinedSession,
    console: Option<&SessionConsole>,
) -> Result<u32, String> {
    let builder = ContainerBuilder::new(spec.name.as_str().to_string(), SyscallType::default())
        .with_root_path(containers_root)
        .map_err(|err| {
            format!("rooting the container state at {}: {err}", containers_root.display())
        })?
        .with_executor(session);

    let joined = match console {
        Some(console) => builder
            .with_console_socket(Some(&console.socket_path))
            .as_tenant()
            .with_process(Some(&console.process_file))
            .with_detach(true)
            .build(),
        None => {
            let environment: HashMap<String, String> = spec.env.iter().cloned().collect();
            builder
                .as_tenant()
                .with_container_args(spec.command.clone())
                .with_cwd(Some(spec.cwd.clone()))
                .with_env(environment)
                .with_detach(true)
                .build()
        }
    };
    let pid =
        joined.map_err(|err| format!("joining a session to '{}': {err}", spec.name.as_str()))?;

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

/// The kernel liveness token of a running process: its pid paired with the inode
/// of its mount namespace, which is what tells the anchor apart from whatever
/// later reuses its pid. Both the sandbox that records a token and the
/// enumeration that reports one read it here, so the two always agree.
fn liveness_token(pid: u32) -> io::Result<LivenessToken> {
    let inode = fs::metadata(format!("/proc/{pid}/ns/mnt"))?.ino();
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
        .set_mounts(Some(sandbox_mounts(&spec.workdir, &spec.mounts)))
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

/// The base mount set, the worktree, the two directories the sandbox writes to
/// without keeping anything, and the host paths it carries in read-only. The
/// rootless base set is the one that works here: it binds the host `/sys`
/// instead of mounting a fresh one, which a user namespace that does not own the
/// network namespace may not do, and it drops the terminal group id, which is
/// not mapped inside the sandbox.
fn sandbox_mounts(workdir: &Path, read_only: &[SandboxMount]) -> Vec<Mount> {
    let mut mounts = get_rootless_mounts();
    let mut worktree = Mount::default();
    worktree.set_destination(PathBuf::from(WORKDIR));
    worktree.set_typ(Some("bind".to_string()));
    worktree.set_source(Some(workdir.to_path_buf()));
    worktree.set_options(Some(vec!["rbind".to_string(), "rw".to_string()]));
    mounts.push(worktree);
    mounts.push(ephemeral_tmpfs(SANDBOX_HOME, "0700"));
    mounts.push(ephemeral_tmpfs(SANDBOX_TMP, "1777"));
    // Last, because the list is applied in order and most of these live inside
    // the home: a tmpfs laid over them afterwards would cover every one.
    mounts.extend(read_only.iter().map(read_only_bind));
    mounts
}

/// A host path the sandbox reads and cannot write. The sandbox holds no
/// capability to remount anything, so what this option denies stays denied.
fn read_only_bind(mount: &SandboxMount) -> Mount {
    let mut bind = Mount::default();
    bind.set_destination(mount.target.clone());
    bind.set_typ(Some("bind".to_string()));
    bind.set_source(Some(mount.source.clone()));
    bind.set_options(Some(vec!["rbind".to_string(), "ro".to_string()]));
    bind
}

/// A RAM backed directory that dies with the sandbox. `noexec` is deliberately
/// absent: the home and `/tmp` are exactly where an agent toolchain unpacks and
/// runs things (npm, pip, an installer that downloads and executes), so refusing
/// that would break ordinary use of the box to gain nothing, since what confines
/// the box is the namespace and not the exec bit.
fn ephemeral_tmpfs(destination: &str, mode: &str) -> Mount {
    let mut tmpfs = Mount::default();
    tmpfs.set_destination(PathBuf::from(destination));
    tmpfs.set_typ(Some("tmpfs".to_string()));
    tmpfs.set_source(Some(PathBuf::from("tmpfs")));
    tmpfs.set_options(Some(vec![
        "nosuid".to_string(),
        "nodev".to_string(),
        format!("mode={mode}"),
    ]));
    tmpfs
}

fn anchor_process(env: &[(String, String)]) -> Process {
    let mut process = Process::default();
    process.set_args(Some(vec!["sleep".to_string(), "infinity".to_string()]));
    process.set_env(Some(environment(env)));
    process.set_capabilities(Some(no_capabilities()));
    process
}

/// The environment every process in the sandbox starts with, the anchor and each
/// session alike. A session that asks for a terminal is described by a process
/// file, and that file replaces the process the tenant API would have built
/// rather than adding to it, so such a session inherits nothing from the
/// sandbox: what is not named here is named nowhere for the shell a person
/// actually types into.
fn environment(pairs: &[(String, String)]) -> Vec<String> {
    // Without PATH the runtime cannot resolve the anchor, which is named without
    // a directory, and refuses to exec at all. Without HOME the mapped uid, which
    // has no passwd entry, falls back to HOME=/ and every tool in the box keeps
    // its history and its caches in the root of the merged overlay. The four XDG
    // variables are the ones the standard derives from HOME.
    let mut environment = vec![
        format!("PATH={DEFAULT_PATH}"),
        format!("HOME={SANDBOX_HOME}"),
        format!("XDG_CONFIG_HOME={SANDBOX_HOME}/.config"),
        format!("XDG_DATA_HOME={SANDBOX_HOME}/.local/share"),
        format!("XDG_STATE_HOME={SANDBOX_HOME}/.local/state"),
        format!("XDG_CACHE_HOME={SANDBOX_HOME}/.cache"),
    ];
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

    use libcontainer::container::ContainerStatus;
    use libcontainer::oci_spec::runtime::{
        Capabilities, LinuxIdMappingBuilder, LinuxNamespaceType,
    };

    use crate::adapters::liveness::ProcLivenessProbe;
    use crate::ports::{LivenessProbe, ResourceLimits, SandboxMount};

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
            mounts: Vec::new(),
            resources: None,
        }
    }

    /// A sandbox carrying one declared host path into the home the sandbox runs
    /// with, which is where the mapping puts anything the user keeps in theirs.
    fn spec_mounting_a_dotfile() -> OciSpec {
        OciSpec {
            mounts: vec![SandboxMount {
                source: PathBuf::from("/home/tester/.config/fish"),
                target: PathBuf::from("/home/hort/.config/fish"),
            }],
            ..sandbox_spec()
        }
    }

    /// Where `destination` sits in the assembled mount list, which is the order
    /// the kernel applies them in.
    fn mount_index(assembled: &Spec, destination: &str) -> usize {
        assembled
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .position(|mount| mount.destination() == &PathBuf::from(destination))
            .unwrap_or_else(|| panic!("a mount at {destination}"))
    }

    /// Where the last RAM backed directory sits in the assembled mount list.
    /// Anything mounted before it that lands underneath it is covered by it.
    fn last_tmpfs_index(assembled: &Spec) -> usize {
        assembled
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .rposition(|mount| mount.typ() == &Some("tmpfs".to_string()))
            .expect("a tmpfs in the mount list")
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
    fn session_process_sets_every_capability_set_empty() {
        let spec = SessionSpec {
            name: SandboxName::new("demo").unwrap(),
            command: vec!["/bin/sh".to_string()],
            cwd: PathBuf::from("/workdir"),
            env: Vec::new(),
            terminal: true,
        };

        // A session that asks for a terminal is the one case the tenant API
        // cannot serve from its own setters, so it is handed a process file
        // instead, and that file replaces the process the API would have built
        // rather than adding to it. The empty set it would have inherited from
        // the sandbox goes with it, so a session on a pty would be the one
        // process in the box holding capabilities.
        let process = session_process(&spec);
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
    fn spec_backs_the_sandbox_home_with_a_tmpfs() {
        let assembled = anchor_spec(&sandbox_spec());

        // An arbitrary uid with no passwd entry has no home, and a home carved
        // out of the merged root would put every cache and history file a tool
        // writes into the overlay upper. This one is RAM backed and goes with the
        // sandbox.
        let home = assembled
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.destination() == &PathBuf::from("/home/hort"))
            .expect("a mount at the sandbox home");
        assert_eq!(home.typ(), &Some("tmpfs".to_string()));
    }

    #[test]
    fn spec_backs_tmp_with_a_tmpfs() {
        let assembled = anchor_spec(&sandbox_spec());

        // The base mount set carries no /tmp at all, so without this one every
        // temporary file the box writes lands in the overlay upper.
        let tmp = assembled
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.destination() == &PathBuf::from("/tmp"))
            .expect("a mount at /tmp");
        assert_eq!(tmp.typ(), &Some("tmpfs".to_string()));
    }

    #[test]
    fn spec_mounts_a_declared_path_read_only() {
        let assembled = anchor_spec(&spec_mounting_a_dotfile());

        // What holds a dotfile and a credential out of the box's reach is this
        // option and nothing else: the sandbox holds no capability to remount
        // anything, so a bind that arrives writable stays writable, and an agent
        // running unrestricted inside the box can rewrite the shell
        // configuration and the credentials of the user who started it.
        let mounts = assembled.mounts().as_ref().unwrap();
        let dotfile = &mounts[mount_index(&assembled, "/home/hort/.config/fish")];
        assert_eq!(dotfile.source(), &Some(PathBuf::from("/home/tester/.config/fish")));
        assert!(dotfile.options().as_ref().unwrap().contains(&"ro".to_string()));
    }

    #[test]
    fn read_only_mounts_come_after_every_tmpfs_that_would_hide_them() {
        let assembled = anchor_spec(&spec_mounting_a_dotfile());

        // The mount list is applied in order, so a tmpfs laid down after a bind
        // that lands underneath it covers that bind: the box comes up missing
        // what the user declared, no error anywhere, and a shell that silently
        // behaves like nobody's. The home is not the only one that can do it,
        // because a declared source outside the user's own home keeps its
        // absolute path and can therefore land under any of them.
        assert!(last_tmpfs_index(&assembled) < mount_index(&assembled, "/home/hort/.config/fish"));
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
    fn spec_exports_home_at_the_dedicated_path() {
        let assembled = anchor_spec(&sandbox_spec());

        // A uid with no passwd entry falls back to HOME=/, so a shell in the box
        // writes its history and its caches into the root of the merged overlay
        // and every tool that keeps state in a home keeps it nowhere.
        let process = assembled.process().as_ref().unwrap();
        let environment = process.env().as_ref().unwrap();
        assert!(environment.contains(&"HOME=/home/hort".to_string()));
    }

    #[test]
    fn spec_points_the_xdg_variables_inside_the_home() {
        let assembled = anchor_spec(&sandbox_spec());

        let process = assembled.process().as_ref().unwrap();
        let environment = process.env().as_ref().unwrap();
        assert!(environment.contains(&"XDG_CONFIG_HOME=/home/hort/.config".to_string()));
        assert!(environment.contains(&"XDG_DATA_HOME=/home/hort/.local/share".to_string()));
        assert!(environment.contains(&"XDG_STATE_HOME=/home/hort/.local/state".to_string()));
        assert!(environment.contains(&"XDG_CACHE_HOME=/home/hort/.cache".to_string()));
    }

    #[test]
    fn session_process_exports_the_sandbox_home_environment() {
        let spec = SessionSpec {
            name: SandboxName::new("demo").unwrap(),
            command: vec!["/bin/sh".to_string()],
            cwd: PathBuf::from("/workdir"),
            env: Vec::new(),
            terminal: true,
        };

        // A session that asks for a terminal is described by a process file, and
        // that file replaces the process the tenant API would have built rather
        // than adding to it: the environment it would have inherited from the
        // sandbox goes with it. That session is the shell a person actually types
        // into, so a home named only in the sandbox spec is a home nobody gets.
        let process = session_process(&spec);
        let environment = process.env().as_ref().unwrap();
        assert!(environment.contains(&"HOME=/home/hort".to_string()));
        assert!(environment.contains(&"XDG_CONFIG_HOME=/home/hort/.config".to_string()));
        assert!(environment.contains(&"XDG_DATA_HOME=/home/hort/.local/share".to_string()));
        assert!(environment.contains(&"XDG_STATE_HOME=/home/hort/.local/state".to_string()));
        assert!(environment.contains(&"XDG_CACHE_HOME=/home/hort/.cache".to_string()));
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
        let runtime_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        let result = runtime.teardown(&SandboxName::new("ghost").unwrap());

        assert!(result.is_ok());
    }

    #[test]
    fn teardown_removes_the_sandbox_runtime_directory() {
        let runtime_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());
        let sandbox_runtime_dir = runtime_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&sandbox_runtime_dir).unwrap();
        fs::write(sandbox_runtime_dir.join("output.log"), b"pasta spoke\n").unwrap();

        runtime.teardown(&SandboxName::new("demo").unwrap()).unwrap();

        // Nothing else comes back for these: they used to be swept along by the
        // removal of the sandbox's state directory, and a runtime directory that
        // no step removes accumulates one log and one pid file per sandbox for as
        // long as the machine is up.
        assert!(!sandbox_runtime_dir.exists());
    }

    #[test]
    fn container_state_and_helper_artifacts_do_not_share_a_directory() {
        let runtime_root = tempfile::tempdir().unwrap();
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());
        let neighbour = runtime_root.path().join("sandboxes").join("demo");
        fs::create_dir_all(&neighbour).unwrap();
        fs::write(neighbour.join("output.log"), b"the anchor spoke\n").unwrap();

        // A name nothing rejects: `SandboxName` refuses only the empty string and
        // a path separator, so a user is free to call a sandbox after the
        // directory the others keep their runtime files in.
        runtime.teardown(&SandboxName::new("sandboxes").unwrap()).unwrap();

        // With the two families in one tree, this one sandbox owns the parent of
        // every other sandbox's runtime files, and tearing it down takes their
        // logs and pid files with it, leaving live helpers nothing can stop.
        assert!(neighbour.join("output.log").exists());
    }

    /// Write the container state a build leaves behind, through the runtime's own
    /// public writer rather than by hand, so what these tests hand the registry is
    /// what a real sandbox writes and no test knows the file format.
    fn record_container(runtime_root: &Path, id: &str, status: ContainerStatus, pid: Option<i32>) {
        let container_dir = runtime_root.join("containers").join(id);
        let bundle = container_dir.join(BUNDLE_DIR);
        fs::create_dir_all(&bundle).unwrap();
        Container::new(id, status, pid, &bundle, &container_dir).unwrap().save().unwrap();
    }

    fn registry_over(runtime_root: &Path) -> LibcontainerRuntime {
        LibcontainerRuntime::new(runtime_root.to_path_buf())
    }

    /// A pid that is certainly alive and certainly readable: this process. What a
    /// live anchor and the test process have in common is the only thing the
    /// registry reads about either.
    fn a_live_pid() -> i32 {
        std::process::id() as i32
    }

    #[test]
    fn registry_reports_a_running_container_as_a_live_anchor() {
        let runtime_root = tempfile::tempdir().unwrap();
        record_container(runtime_root.path(), "demo", ContainerStatus::Running, Some(a_live_pid()));
        let registry = registry_over(runtime_root.path());

        let live = registry.list_live().unwrap();

        // Nothing else in hort answers "which anchors are up". A registry that
        // finds none reports every sandbox in `ls` as orphaned while its anchor
        // runs, and hands `prune` a live box as debris to remove.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, SandboxName::new("demo").unwrap());
    }

    #[test]
    fn registry_reports_a_token_the_liveness_probe_recognizes() {
        let runtime_root = tempfile::tempdir().unwrap();
        record_container(runtime_root.path(), "demo", ContainerStatus::Running, Some(a_live_pid()));
        let registry = registry_over(runtime_root.path());

        let live = registry.list_live().unwrap();

        // Reconciliation matches this token against the one the record carries,
        // and that one was read the way this probe reads it. A registry that
        // reports the right sandbox under a token nobody recognizes is the same
        // outcome as reporting nothing, and looks correct from every other angle.
        assert!(ProcLivenessProbe.is_alive(&live[0].token));
    }

    #[test]
    fn registry_skips_a_container_directory_it_cannot_read() {
        let runtime_root = tempfile::tempdir().unwrap();
        record_container(runtime_root.path(), "demo", ContainerStatus::Running, Some(a_live_pid()));
        fs::create_dir_all(runtime_root.path().join("containers").join("half-written")).unwrap();

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The registry reads state another process wrote and the kernel may have
        // outlived, so a directory it cannot make sense of is an ordinary state,
        // one an interrupted build or a container removed mid-walk leaves behind.
        // Failing on it would take down every command that reconciles, and the
        // only way out would be deleting files by hand.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, SandboxName::new("demo").unwrap());
    }

    #[test]
    fn registry_omits_a_container_whose_anchor_is_not_running() {
        let runtime_root = tempfile::tempdir().unwrap();
        // A live pid with a status short of running: the runtime keeps that
        // status rather than promoting it, so nothing but the status itself
        // stands between this entry and being read as a live anchor. Recording
        // it with a dead pid instead would prove nothing, because a pid nobody
        // can read is dropped a step later anyway.
        record_container(runtime_root.path(), "demo", ContainerStatus::Created, Some(a_live_pid()));

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The container state outlives the anchor it describes, so the directory
        // alone means nothing. Reading it as alive makes `ls` report a dead
        // sandbox as live and keeps `prune` from ever offering to clean it.
        assert!(live.is_empty());
    }

    #[test]
    fn registry_refuses_a_root_it_cannot_read_rather_than_reporting_an_empty_one() {
        let runtime_root = tempfile::tempdir().unwrap();
        // Anything but a directory where the container states belong: reading it
        // fails with a kind that is not "nothing here yet", which is the whole
        // distinction.
        fs::write(runtime_root.path().join("containers"), b"not a directory\n").unwrap();
        let registry = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        let live = registry.list_live();

        // An unreadable root answered as an empty one is how every running
        // sandbox on the machine is handed to `prune` as debris at once. Absent
        // is knowledge; unreadable is not.
        assert!(live.is_err());
    }

    #[test]
    fn registry_reports_nothing_when_no_container_was_ever_built() {
        let runtime_root = tempfile::tempdir().unwrap();
        let registry = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        let live = registry.list_live().unwrap();

        // Nothing has run yet: the root is created by the first build, and every
        // command that reconciles asks this before then.
        assert!(live.is_empty());
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
    use crate::ports::{DbForward, EnvironmentProbe, NetworkProvider, NetworkSpec, SandboxMount};

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
            mounts: Vec::new(),
            resources: None,
        }
    }

    fn runtime_under(runtime_root: &TempDir) -> LibcontainerRuntime {
        LibcontainerRuntime::new(runtime_root.path().to_path_buf())
    }

    /// A session that stays alive long enough to be read from the host, which is
    /// the only way a test can ask where a session ended up.
    fn session_spec(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
        }
    }

    /// The directory the sandbox's helpers keep their runtime files in: the log,
    /// the pid files, and the record of the ports a session may reach.
    fn sandbox_dir(runtime_root: &Path) -> PathBuf {
        runtime_root.join("sandboxes/demo")
    }

    /// The file a session leaves in `/workdir` when it finds a terminal on its
    /// input. `/workdir` is a bind mount of a host directory, so what the session
    /// writes there is what a test outside the sandbox can read.
    const TERMINAL_WITNESS: &str = "ran-on-a-terminal";

    /// A session that asks for a terminal and reports from inside whether it got
    /// one.
    fn reporting_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("test -t 0 && touch {WORKDIR}/{TERMINAL_WITNESS}; sleep infinity"),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: true,
        }
    }

    fn appeared_within_deadline(path: &Path) -> bool {
        let deadline = Instant::now() + SESSION_DEADLINE;
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            sleep(POLL);
        }
        false
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

    /// The file a session leaves in whatever `HOME` names it.
    const HOME_WITNESS: &str = "wrote-in-the-sandbox-home";

    /// A session that writes in its own home and stays alive afterwards. It names
    /// the directory the way a tool inside the box does, through the variable, so
    /// what it writes says both that the variable arrived and where it pointed.
    fn home_writing_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("touch \"$HOME/{HOME_WITNESS}\"; sleep infinity"),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
        }
    }

    /// The dotfile a sandbox carries in from the host, and the line in it. The
    /// content has no trailing newline so a session can compare it against what
    /// a command substitution gives back.
    const DOTFILE: &str = "config.fish";
    const DOTFILE_CONTENT: &str = "set -g fish_color_command blue";
    /// Where the mapping puts a dotfile the user keeps under their own home.
    const MOUNTED_DOTFILE_DIR: &str = "/home/hort/.config/fish";
    /// The file a session leaves when the dotfile it read holds what the host
    /// wrote.
    const DOTFILE_WITNESS: &str = "read-the-dotfile";
    /// The file a session leaves when the mount refused its write.
    const REFUSAL_WITNESS: &str = "refused-the-write";

    /// A host directory holding one dotfile, and the mount that carries it into
    /// the sandbox home.
    fn dotfile_mount(state_root: &Path) -> SandboxMount {
        let source = state_root.join("dotfiles").join("fish");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join(DOTFILE), DOTFILE_CONTENT).unwrap();
        SandboxMount { source, target: PathBuf::from(MOUNTED_DOTFILE_DIR) }
    }

    /// A session that reads the mounted dotfile and reports into `/workdir`
    /// whether it held what the host put there. It leaves rather than sleeps:
    /// nothing here needs it alive, and a session left running holds hort's own
    /// output open.
    fn dotfile_reading_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "test \"$(cat {MOUNTED_DOTFILE_DIR}/{DOTFILE})\" = \"{DOTFILE_CONTENT}\" && touch {WORKDIR}/{DOTFILE_WITNESS}"
                ),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
        }
    }

    /// The file a session tries to write into the directory the mount carried
    /// in.
    const WRITE_PROBE: &str = "probe";

    /// A session that writes into the mounted directory and reports into
    /// `/workdir` when the write was refused.
    ///
    /// It checks the directory is there before it writes, and that guard is what
    /// makes this discriminating rather than decorative: with no mount at all
    /// the home is an empty tmpfs, so the write fails for want of a directory
    /// and reports a refusal that never happened.
    fn write_attempting_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "test -d {MOUNTED_DOTFILE_DIR} && ! touch {MOUNTED_DOTFILE_DIR}/{WRITE_PROBE} 2>/dev/null && touch {WORKDIR}/{REFUSAL_WITNESS}"
                ),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
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
            terminal: false,
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        let anchor_mnt_ns = fs::metadata(format!("/proc/{}/ns/mnt", token.pid.0)).unwrap();
        assert_eq!(token.mnt_ns.0, anchor_mnt_ns.ino());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn registry_reports_the_token_the_anchor_was_started_under() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let started = runtime.start_anchor(&spec).unwrap();

        let live = runtime.list_live().unwrap();

        // The two halves of reconciliation are written by different code at
        // different times: `up` records this token, and this read produces the
        // one it is matched against. Only a real anchor can say they agree, since
        // only here does the pid come from the runtime's own state file rather
        // than from the test. Whole-token equality is the contract: same pid
        // under an inode read another way still reads as a dead sandbox.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, spec.name);
        assert_eq!(live[0].token, started);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn registry_stops_reporting_a_sandbox_whose_anchor_was_killed() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let started = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(started.pid.0);
        unsafe { libc::kill(started.pid.0 as libc::pid_t, libc::SIGKILL) };
        assert!(stopped_within_deadline(started.pid.0), "the anchor outlived the kill");

        let live = runtime.list_live().unwrap();

        // The container state survives the anchor, and hort is built to reconcile
        // against the kernel rather than to prevent the kill. A registry reading
        // the leftover state as a live anchor is what would make `ls` insist a
        // killed sandbox is running and keep `prune` from clearing the debris.
        assert!(live.is_empty());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_runs_with_an_empty_capability_set() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        // The anchor outlives the command that started it, so an anchor left
        // holding the streams hort was invoked with holds them for the life of
        // the sandbox: a piped or redirected invocation never reaches EOF and its
        // reader waits on a `sleep infinity` that will never write.
        wait_for_anchor(token.pid.0);
        let log = sandbox_log_path(&sandbox_dir(runtime_root.path()));
        assert_eq!(fs::read_link(format!("/proc/{}/fd/1", token.pid.0)).unwrap(), log);
        assert_eq!(fs::read_link(format!("/proc/{}/fd/2", token.pid.0)).unwrap(), log);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_state_root_holds_no_sandbox_log() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());

        let token = runtime.start_anchor(&spec).unwrap();

        // The log is written by processes the distribution labels, and the state
        // root carries a label those processes may not write to, so the one file
        // hort diagnoses a failed sandbox from is also the one the policy silences.
        // Writing it in both places would keep that failure and hand the reader two
        // halves of one account.
        wait_for_anchor(token.pid.0);
        assert!(!sandbox_log_path(&state_root.path().join("sandboxes/demo")).exists());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_anchor_reads_from_nothing_rather_than_from_what_invoked_hort() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();

        assert_eq!(namespace_inode(session.pid, "mnt"), namespace_inode(token.pid.0, "mnt"));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn session_runs_in_the_sandbox_network_namespace() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();

        // A session inherits the network namespace of the process that opened
        // it, and the sandbox's spec declares none for the tenant API to find,
        // so a session opened straight from hort lands on the host network. It
        // sees the same worktree, runs the same shell and answers every other
        // question here correctly, while the egress allowlist restricts nothing.
        assert_eq!(namespace_inode(session.pid, "net"), namespace_inode(token.pid.0, "net"));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_finds_a_writable_home_at_the_dedicated_path() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        runtime.join_session(&home_writing_session(&spec.name)).unwrap();

        // Three things no assembled spec can answer on its own: that the kernel
        // takes the mount, that the runtime creates a directory the base rootfs
        // never had, and that the home the sandbox exports is the home a session
        // joined afterwards runs with. The prepared rootfs holds no /home/hort,
        // so the file exists only if all three held.
        let written = PathBuf::from(format!("/proc/{}/root/home/hort/{HOME_WITNESS}", token.pid.0));
        assert!(appeared_within_deadline(&written));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_reads_a_dotfile_mounted_from_the_host() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = OciSpec {
            mounts: vec![dotfile_mount(state_root.path())],
            ..sandbox_spec(rootfs, state_root.path())
        };
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        runtime.join_session(&dotfile_reading_session(&spec.name)).unwrap();

        // This is the whole point of the feature and no assembled spec can stand
        // in for it: the destination lives inside a tmpfs the base rootfs never
        // had, the kernel has to take the bind under a mapping that owns
        // neither side, and what the session reads has to be the bytes the user
        // has on their own machine.
        assert!(appeared_within_deadline(&spec.workdir.join(DOTFILE_WITNESS)));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_cannot_write_to_a_read_only_mount() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = OciSpec {
            mounts: vec![dotfile_mount(state_root.path())],
            ..sandbox_spec(rootfs, state_root.path())
        };
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        runtime.join_session(&write_attempting_session(&spec.name)).unwrap();

        // The mount is what denies this, not a later layer: these are the user's
        // own dotfiles and credentials on the user's own disk, reached by an
        // agent the box exists to run with every permission granted.
        assert!(appeared_within_deadline(&spec.workdir.join(REFUSAL_WITNESS)));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_that_asked_for_a_terminal_runs_on_one() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        // Held for as long as the answer is waited for: closing the master hangs
        // the session's terminal up, and a session hung up before it has run
        // answers nothing about the terminal it was given.
        let _session = runtime.join_session(&reporting_session(&spec.name)).unwrap();

        // What the agent inside sees has to be a terminal or the interactive
        // tools the sandbox exists to run refuse to draw anything, and the pty
        // that terminal is made of has to be the sandbox's own.
        assert!(appeared_within_deadline(&spec.workdir.join(TERMINAL_WITNESS)));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_master_of_a_session_terminal_reaches_hort() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        let session = runtime.join_session(&reporting_session(&spec.name)).unwrap();

        // The runtime only sends the master; hort has to be the one listening
        // for it, and a session whose master never arrives is a terminal nobody
        // can relay, which is a shell the user cannot type into.
        let master = session.pty.expect("the pty master of a session that asked for a terminal");
        assert_eq!(unsafe { libc::isatty(master.as_raw_fd()) }, 1);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_gets_its_terminal_under_a_deep_runtime_root() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        // A runtime root as deep as one gets: the variable that names it is the
        // user's to set, and a session bus or a container tool that points it at a
        // path of its own is enough. Every test path in this repo lives directly
        // under /tmp and is therefore short, so nothing else here can tell a socket
        // address that fits from one that does not.
        let deep = runtime_root
            .path()
            .join("runtime-dir-on-a-long-organisation-path")
            .join("with-a-nested-hort-root");
        fs::create_dir_all(&deep).unwrap();
        let runtime = LibcontainerRuntime::new(deep.clone());
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        // A Unix socket address is capped at 107 usable bytes, so a console
        // socket named from the runtime root spends that budget on the depth of
        // the path and leaves the rest to the sandbox name. hort listens on this
        // one itself, and the limit binds both ends independently: the runtime it
        // embeds dodges it on its own connect by pointing a short symlink at the
        // long path, which does nothing for the address hort binds.
        let session = runtime.join_session(&reporting_session(&spec.name)).unwrap();

        assert!(session.pty.is_some());
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(runtime_root.path().to_path_buf());
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
        let sandbox_dir = sandbox_dir(runtime_root.path());
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(runtime_root.path().to_path_buf());
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let spec = sandbox_spec(rootfs, state_root.path());
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = PastaNetworkProvider::new(runtime_root.path().to_path_buf());
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
        let runtime_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = runtime_under(&runtime_root);
        let absent = state_root.path().join("no-such-rootfs");
        let spec = sandbox_spec(absent, state_root.path());

        let result = runtime.start_anchor(&spec);

        assert!(matches!(result, Err(HortError::ContainerRuntimeFailed { .. })));
    }
}
