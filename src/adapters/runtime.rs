//! The container half of a sandbox: `LibcontainerRuntime`, the embedded OCI
//! runtime that starts and stops the anchor, joins sessions to it, and reads
//! back which processes a sandbox is holding.
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
//! Those files are bookkeeping the anchor does not depend on, though, so the
//! process table is read alongside them: a process of this user carrying the
//! sandbox marker is an anchor as well, and an anchor no file describes any more
//! is precisely the one whose worktree must not be deleted under it.
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
use std::thread::sleep;
use std::time::{Duration, Instant};

use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::{Container, ContainerStatus};
use libcontainer::oci_spec::runtime::{
    Arch, Capabilities, Linux, LinuxCapabilities, LinuxCpu, LinuxIdMapping, LinuxIdMappingBuilder,
    LinuxMemory, LinuxNamespace, LinuxNamespaceType, LinuxResources, LinuxSeccomp,
    LinuxSeccompAction, LinuxSeccompArg, LinuxSyscall, Mount, Process, Root, Spec,
    get_rootless_mounts,
};
use libcontainer::seccomp::initialize_seccomp;
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::default::DefaultExecutor;
use libcontainer::workload::{Executor, ExecutorError, ExecutorValidationError};
use serde::Deserialize;

use crate::adapters::console;
use crate::adapters::landlock;
use crate::adapters::namespaces::{enter, owning_user_namespace};
use crate::adapters::streams::{open_sandbox_log, sandbox_log_path};
use crate::domain::error::HortError;
use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxName};
use crate::domain::mounts::{SANDBOX_HOME, WORKDIR};
use crate::ports::{
    ContainerRegistry, ContainerRuntime, MountAccess, OciSpec, RegistryEntry, ResourceLimits,
    SandboxFile, SandboxMount, Session, SessionProbe, SessionSpec,
};

const SANDBOXES_DIR: &str = "sandboxes";
const CONTAINERS_DIR: &str = "containers";
const BUNDLE_DIR: &str = "bundle";
const CONSOLE_SUFFIXES: [&str; 2] = [".console", ".process.json"];
const CONFIG_FILE: &str = "config.json";
const UPPER_LAYER: &str = "upper";
const WORK_LAYER: &str = "work";
const MERGED_ROOT: &str = "merged";
const SANDBOX_TMP: &str = "/tmp";
const DEV_NULL: &str = "/dev/null";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CPU_PERIOD_USEC: u64 = 100_000;
/// Where the embedded runtime puts a container's cgroup: it holds this root as a
/// constant of its own rather than looking the mount up, so a reader that went
/// hunting for cgroup2 in the mount table could end up somewhere nothing was
/// ever written.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const CGROUP_PROCS_FILE: &str = "cgroup.procs";
const PROC_ROOT: &str = "/proc";
/// The environment entry hort puts in a sandbox, and the whole of what tells a
/// sandbox's processes from the rest of what this user is running.
const SANDBOX_MARKER: &str = "HORT_SANDBOX=";
/// The line of `/proc/<pid>/status` numbering a process in every pid namespace
/// it belongs to, the reader's own first and the one it lives in last.
const PID_NUMBERING: &str = "NSpid:";
/// What the first process of a pid namespace is numbered inside it.
const FIRST_PROCESS: &str = "1";
/// How long a sandbox's own processes are given to answer a request to stop
/// before the sandbox comes down over them.
const STOP_GRACE: Duration = Duration::from_secs(5);
const STOP_POLL: Duration = Duration::from_millis(50);
/// What every line of `/proc/<pid>/cgroup` starts with on a unified host:
/// hierarchy id zero and an empty controller list, then the path.
const UNIFIED_HIERARCHY: &str = "0::";
/// The byte both handshakes send; only its arrival, never its value, carries
/// meaning, and the closed pipe that yields none is the abort signal.
const HANDSHAKE: [u8; 1] = [1];
const PROCESS_FAILED: u8 = 0;
const PROCESS_STARTED: u8 = 1;

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
        if !self.has_container_state(name) {
            // Those files are bookkeeping the anchor does not depend on, so losing
            // them costs the runtime the handle it stops a container by and costs
            // the container nothing at all. What is left of the sandbox is
            // whatever the kernel still has of it, and a signal is the only way
            // left to reach that.
            return stop_by_signal(name);
        }

        let mut container = Container::load(self.container_dir(name)).map_err(|err| {
            runtime_failure(format!(
                "teardown: loading the container state of '{}': {err}",
                name.as_str()
            ))
        })?;
        // A delete that fails has lost the handle, not answered the question: it
        // races systemd's collection of the sandbox's scope, and from either side
        // of that race the runtime can no longer say whether anything is still
        // standing. Asking it a second time is worse than useless, because that
        // call re-reads the anchor's state first, an anchor still on its way out
        // reads as running, and it goes down a branch the first call never took.
        // The kernel is the one source left that tells a sandbox already gone from
        // one still holding the worktree mounted.
        if container.delete(true).is_err() {
            stop_by_signal(name)?;
            // The delete that failed never reached the point where it takes its
            // own state away, and that state is what refuses the next build of
            // this name. Taking it is only ever reached once the kernel has
            // answered that nothing of the sandbox is left, and one that will not
            // go is no reason to fail the teardown of a container already gone.
            let _ = fs::remove_dir_all(self.container_dir(name));
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
        // Before the merge and not after it: a write into a layer the merge is
        // already assembled from is not guaranteed to show through it.
        write_drop_ins(&spec.overlay, &spec.drop_ins)?;
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
    /// runtime keeps, or from the process table once that state is gone. It is
    /// the anchor's namespaces a session climbs into and the anchor's cgroup a
    /// sandbox's processes are counted in, and naming it here is what spares
    /// both callers from carrying it around.
    ///
    /// The state stays the first source because reading it is one exact file
    /// read, where the process table is a walk of `/proc` that only a sandbox
    /// whose state has been lost should have to pay for.
    fn anchor_pid(&self, name: &SandboxName, operation: &str) -> Result<u32, HortError> {
        if !self.has_container_state(name) {
            return declared_anchor_pid(name, operation);
        }
        let container = Container::load(self.container_dir(name)).map_err(|err| {
            runtime_failure(format!(
                "{operation}: loading the container state of '{}': {err}",
                name.as_str()
            ))
        })?;
        let pid = container.pid().ok_or_else(|| {
            runtime_failure(format!("{operation}: sandbox '{}' has no anchor", name.as_str()))
        })?;
        u32::try_from(pid.as_raw()).map_err(|_| {
            runtime_failure(format!(
                "{operation}: the runtime reported {} as the anchor pid",
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

    /// Whether this sandbox's container directory still stands under the runtime
    /// root, which is where the embedded runtime keeps everything it loads a
    /// container back from and where a session's join sockets are created.
    ///
    /// One read answers three askers: the join's precondition, the teardown's
    /// choice between the runtime's own delete and a signal, and the anchor
    /// lookup's choice between the state file and the process table. Asking the
    /// same question in all three is what makes the sandbox `attach` refuses
    /// exactly the one `down` still collects by signal and `ls` still counts.
    fn has_container_state(&self, name: &SandboxName) -> bool {
        self.container_dir(name).exists()
    }

    fn join_session(&self, spec: &SessionSpec) -> Result<Session, HortError> {
        let anchor = self.anchor_pid(&spec.name, "join_session")?;
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
        // Read before the walk below can answer anything, because the anchor
        // this source exists for is exactly the one the walk will not find.
        let declared = declared_anchors();

        let containers_root = self.containers_root();
        let container_dirs = match fs::read_dir(&containers_root) {
            Ok(entries) => entries,
            // Nothing was ever built under this root, which says where the
            // container states are not and nothing about which anchors are up.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(declared),
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

        let recorded =
            container_dirs.flatten().filter_map(|entry| live_anchor(&entry.path())).collect();
        Ok(merged(recorded, declared))
    }
}

impl SessionProbe for LibcontainerRuntime {
    fn session_pids(&self, name: &SandboxName) -> Result<Vec<u32>, HortError> {
        let anchor = self.anchor_pid(name, "session_pids")?;
        let processes = cgroup_of(anchor).map(processes_in).unwrap_or_default();
        Ok(sessions_among(processes, anchor))
    }
}

/// Where the kernel has `pid` right now, as a directory under the cgroup root,
/// or `None` when it has it nowhere any more.
///
/// Asked of the kernel rather than assembled from the sandbox name, because a
/// rootless container is put in a systemd scope and the name of that scope is
/// systemd's convention rather than anything hort chose: rebuilding it here
/// would be a guess that only looks like code. The kernel already knows, and
/// answers in one file read.
///
/// The path it answers with is relative to the cgroup namespace of whoever is
/// reading, not of the process being asked about. hort's own process is never
/// inside a sandbox, so what comes back is the whole path from the root of the
/// hierarchy and joins onto the cgroup mount unchanged.
fn cgroup_of(pid: u32) -> Option<PathBuf> {
    let memberships = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = memberships.lines().find_map(|line| line.strip_prefix(UNIFIED_HIERARCHY))?;
    Some(Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
}

/// Every process the kernel currently has in `cgroup`.
///
/// A cgroup that has just been collected, and a line that is not a pid, both
/// read as nothing rather than as a failure. The question being asked is who is
/// in there at this instant, and the kernel is rewriting the answer as processes
/// come and go, so a read that arrives a moment too late has observed something
/// ordinary.
fn processes_in(cgroup: PathBuf) -> Vec<u32> {
    fs::read_to_string(cgroup.join(CGROUP_PROCS_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// The sessions among the processes a sandbox holds: everything the read found,
/// less the anchor that keeps the box alive. An anchor the read no longer finds
/// leaves the rest of the list exactly as it came.
fn sessions_among(processes: Vec<u32>, anchor: u32) -> Vec<u32> {
    processes.into_iter().filter(|process| *process != anchor).collect()
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

/// Every live anchor the host's own process table names.
///
/// A container's state directory is bookkeeping the anchor does not depend on:
/// an older build wrote it where this one does not look, and a hand that removed
/// it loses it outright, while the process goes on holding the worktree mounted.
/// What no such loss reaches is the anchor's own environment, so the process
/// table answers for the sandboxes those files no longer describe. Without it
/// they are invisible to every command that reconciles, which is what lets
/// `prune` remove a worktree out from under a running container.
///
/// A `/proc` that cannot be listed yields nothing rather than an error, because
/// by then every liveness read in hort is already dead and this enumeration is
/// not where that gets reported.
fn declared_anchors() -> Vec<RegistryEntry> {
    let user = unsafe { libc::getuid() };
    let Ok(processes) = fs::read_dir(PROC_ROOT) else {
        return Vec::new();
    };
    processes
        .flatten()
        .filter_map(|process| process.file_name().to_str()?.parse().ok())
        .filter_map(|pid| declared_anchor(pid, user))
        .collect()
}

/// The live anchor the process at `pid` declares itself to be, or `None` when it
/// declares none.
///
/// Reading a process is racing it, and every step here can find it gone, find a
/// file it may not read, or find a value it cannot make sense of. All of those
/// mean the same thing, that there is no anchor here: a pid that disappears
/// between the listing and the read is the ordinary case rather than a failure
/// of the enumeration, and one process nobody can read is no reason to stop
/// answering for the rest.
///
/// Only what `user` runs is considered. Another user's environment is not
/// readable anyway while hort runs unprivileged, and a sandbox hort could not
/// have started is not one it may report on and offer to clean.
fn declared_anchor(pid: u32, user: u32) -> Option<RegistryEntry> {
    if fs::metadata(format!("{PROC_ROOT}/{pid}")).ok()?.uid() != user {
        return None;
    }
    let environ = fs::read(format!("{PROC_ROOT}/{pid}/environ")).ok()?;
    let id = declared_sandbox(&environ)?;
    let status = fs::read_to_string(format!("{PROC_ROOT}/{pid}/status")).ok()?;
    if !anchors_the_sandbox_it_declares(&status) {
        return None;
    }
    Some(RegistryEntry { id, token: liveness_token(pid).ok()? })
}

/// The sandbox a process says it belongs to, read out of its environment.
///
/// The entries come separated by nul bytes and are not text: an environment may
/// hold bytes no string does, so a value becomes a name only once it has been
/// found. The whole `NAME=` is matched rather than the name alone, or a variable
/// that merely begins the same way would name a sandbox.
fn declared_sandbox(environ: &[u8]) -> Option<SandboxName> {
    let declared = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(SANDBOX_MARKER.as_bytes()))?;
    SandboxName::new(std::str::from_utf8(declared).ok()?).ok()
}

/// Whether the process this status describes is the one its sandbox was started
/// with, rather than one that joined the sandbox afterwards.
///
/// Every session carries the same marker as its anchor and runs in the anchor's
/// mount namespace, so the two differ in nothing read here except the pid, while
/// reconciliation matches a record against the whole token. A session reported
/// in its anchor's place therefore reads a running sandbox as orphaned, which is
/// the very outcome this second source exists to prevent. What separates them is
/// that hort gives a sandbox a pid namespace of its own: the anchor is the first
/// process in it and everything joined later is numbered after it.
fn anchors_the_sandbox_it_declares(status: &str) -> bool {
    let Some(numbering) = status.lines().find_map(|line| line.strip_prefix(PID_NUMBERING)) else {
        return false;
    };
    let mut inside = numbering.split_whitespace();
    inside.next();
    // A process numbered in no namespace but the reader's own is inside no
    // sandbox, so there is nothing here to mistake for a session and the marker
    // it carries is the whole of what it is judged by.
    inside.last().is_none_or(|innermost| innermost == FIRST_PROCESS)
}

/// The two sources of live anchors as one enumeration, one entry per sandbox.
///
/// They overlap for every sandbox in good health, so concatenating them would
/// report the ordinary case twice and offer a box with no record for adoption
/// once per source. What `recorded` says wins where both know a sandbox: it is
/// read from the state the sandbox was started against, which is the state the
/// record was written from.
fn merged(recorded: Vec<RegistryEntry>, declared: Vec<RegistryEntry>) -> Vec<RegistryEntry> {
    let mut anchors = recorded;
    for found in declared {
        if !anchors.iter().any(|known| known.id == found.id) {
            anchors.push(found);
        }
    }
    anchors
}

/// Stop a sandbox by signalling what the kernel still has of it, for when the
/// runtime has no container state left to stop it through.
///
/// A name no live anchor declares is a sandbox that really is down, and saying so
/// is what lets a teardown run a second time. An anchor that is still up is the
/// opposite case: reporting a sandbox down while it holds the worktree mounted
/// gets that worktree deleted under it, which is the corruption the order of a
/// shutdown exists to prevent.
fn stop_by_signal(name: &SandboxName) -> Result<(), HortError> {
    let Some(anchor) = anchors_of(name).into_iter().next() else {
        return Ok(());
    };

    let sessions = sessions_of(&anchor);
    for session in &sessions {
        ask_to_stop(*session);
    }
    wait_until_gone(&sessions, &anchor);

    // Nothing is asked of the anchor, because there is nothing it could hear: it
    // is the first process of the sandbox's own pid namespace, and for such a
    // process the kernel discards a signal whose action is the default unless it
    // installed a handler, which the `sleep` holding a sandbox open never does.
    // Killing it is also what ends a session that refused the request above,
    // since the death of that first process takes its pid namespace with it.
    kill_outright(anchor.pid.0)
}

/// The live anchors the process table says hold `name` open: every process of
/// this user that declares the sandbox and heads a pid namespace of its own.
///
/// The enumeration behind this is deliberately looser, so that a stray process
/// carrying the marker still shows up as something to look at. Strictness
/// belongs here, where getting it wrong signals a process a user is typing in,
/// or counts the anchor in that session's place: every session declares the
/// sandbox exactly as its anchor does, and the pid namespace is the one thing
/// telling them apart that no process can claim.
fn anchors_of(name: &SandboxName) -> Vec<LivenessToken> {
    declared_anchors()
        .into_iter()
        .filter(|entry| entry.id == *name && heads_its_own_pid_namespace(entry.token.pid.0))
        .map(|entry| entry.token)
        .collect()
}

/// The pid of the anchor the process table says holds `name` open, for when the
/// runtime has no container state left to read it from.
///
/// Two anchors under one name is not something to choose between: counted, the
/// answer would be one sandbox's sessions offered as the other's, and entered,
/// a session would climb into whichever the listing happened to put first.
fn declared_anchor_pid(name: &SandboxName, operation: &str) -> Result<u32, HortError> {
    let mut anchors = anchors_of(name).into_iter();
    let Some(anchor) = anchors.next() else {
        return Err(runtime_failure(format!(
            "{operation}: sandbox '{}' has no anchor",
            name.as_str()
        )));
    };
    if anchors.next().is_some() {
        return Err(runtime_failure(format!(
            "{operation}: more than one anchor declares sandbox '{}'",
            name.as_str()
        )));
    }
    Ok(anchor.pid.0)
}

/// Whether the process at `pid` is the first process of a pid namespace of its
/// own, which is what a sandbox anchor is and what nothing else on this host is.
///
/// Any process a user runs may export the variable that names a sandbox, and
/// that alone is enough to be enumerated as an anchor. The pid namespace is not
/// something a process can claim: hort gives one to every sandbox it builds, and
/// a process outside a sandbox is numbered in no namespace but the one doing the
/// reading. A process this cannot be read for is not one to signal.
fn heads_its_own_pid_namespace(pid: u32) -> bool {
    let Ok(status) = fs::read_to_string(format!("{PROC_ROOT}/{pid}/status")) else {
        return false;
    };
    let Some(numbering) = status.lines().find_map(|line| line.strip_prefix(PID_NUMBERING)) else {
        return false;
    };
    let mut inside = numbering.split_whitespace();
    inside.next();
    inside.last().is_some_and(|innermost| innermost == FIRST_PROCESS)
}

/// The sandbox's own processes, less the anchor holding it open.
///
/// The cgroup says where to look and never who belongs. It is the anchor's own,
/// so it holds the sandbox and nothing else, but the same read from a pid that is
/// no container anchor answers a scope shared with the rest of what this user
/// runs, the shell hort was typed into included. The mount namespace is what
/// settles it: every session shares the anchor's by construction, and nothing
/// outside the sandbox is in it.
fn sessions_of(anchor: &LivenessToken) -> Vec<u32> {
    let processes = cgroup_of(anchor.pid.0).map(processes_in).unwrap_or_default();
    sessions_among(processes, anchor.pid.0)
        .into_iter()
        .filter(|pid| joined_to(*pid, anchor))
        .collect()
}

/// Whether the kernel still has `pid`, and has it inside the sandbox `anchor`
/// holds open.
fn joined_to(pid: u32, anchor: &LivenessToken) -> bool {
    liveness_token(pid).is_ok_and(|found| found.mnt_ns == anchor.mnt_ns)
}

/// Wait for every one of `sessions` to leave, returning as soon as none is left
/// and giving up once the grace runs out.
fn wait_until_gone(sessions: &[u32], anchor: &LivenessToken) {
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if !sessions.iter().any(|pid| joined_to(*pid, anchor)) {
            return;
        }
        sleep(STOP_POLL);
    }
}

/// Ask a process to stop, and let a request that could not be delivered pass.
///
/// Asking is a courtesy and never the mechanism. A session can be an agent
/// halfway through writing a file into the worktree, and the difference between a
/// whole file and a truncated one is why a shutdown has an order at all; what
/// takes the sandbox down is the kill that follows, so a request that found
/// nobody has already produced what it was sent for.
fn ask_to_stop(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

/// Take a process down, counting one that has already left as taken down.
fn kill_outright(pid: u32) -> Result<(), HortError> {
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } == -1 {
        let failure = io::Error::last_os_error();
        if failure.raw_os_error() != Some(libc::ESRCH) {
            return Err(runtime_failure(format!(
                "teardown: killing the anchor ({pid}): {failure}"
            )));
        }
    }
    Ok(())
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

/// Put the files the sandbox is given into its own writable layer, making the
/// directories they sit in.
///
/// They go into the layer rather than into the merged root because a bind mount
/// cannot reach these paths: the directories they sit in exist in no prepared
/// rootfs, and a bind creates no parent. Writing into the layer the merged root
/// is assembled from leaves the base untouched and the file visible inside the
/// box.
fn write_drop_ins(overlay: &Path, files: &[SandboxFile]) -> Result<(), String> {
    let upper = overlay.join(UPPER_LAYER);
    for file in files {
        // The leading separator has to go: joining an absolute path onto another
        // keeps only the absolute one, which would put the file on the host's own
        // /etc instead of the sandbox's.
        let landing = upper.join(file.path.strip_prefix("/").unwrap_or(&file.path));
        if let Some(directory) = landing.parent() {
            fs::create_dir_all(directory)
                .map_err(|err| format!("creating {}: {err}", directory.display()))?;
        }
        fs::write(&landing, &file.content)
            .map_err(|err| format!("writing {}: {err}", landing.display()))?;
    }
    Ok(())
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
///
/// The syscall filter is one of those restrictions, loaded here rather than
/// declared on the session, because joining a session rebuilds the container's
/// linux section out of the namespaces it finds plus a handful of fields, and the
/// filter is not one of them however the session was described.
#[derive(Clone)]
struct ConfinedSession {
    connect_ports: Option<Vec<u16>>,
}

impl Executor for ConfinedSession {
    fn exec(&self, spec: &Spec) -> Result<(), ExecutorError> {
        landlock::restrict_session(self.connect_ports.as_deref()).map_err(ExecutorError::Other)?;
        // Loaded here and never earlier: the climb into the sandbox needs setns,
        // which the profile both allows and refuses, so a filter loaded before it
        // rests on which of the two rules went in first and turns every attach
        // into a bare "operation not permitted" the day anything reorders them.
        initialize_seccomp(&sandbox_seccomp()).map(|_| ()).map_err(|err| {
            ExecutorError::Other(format!("loading the sandbox syscall filter: {err}"))
        })?;
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
fn sandbox_mounts(workdir: &Path, declared: &[SandboxMount]) -> Vec<Mount> {
    let mut mounts = get_rootless_mounts();
    let mut worktree = Mount::default();
    worktree.set_destination(PathBuf::from(WORKDIR));
    worktree.set_typ(Some("bind".to_string()));
    worktree.set_source(Some(workdir.to_path_buf()));
    worktree.set_options(Some(vec!["rbind".to_string(), "rw".to_string()]));
    mounts.push(worktree);
    mounts.push(ephemeral_tmpfs(SANDBOX_HOME, "0700"));
    mounts.push(ephemeral_tmpfs(SANDBOX_TMP, "1777"));
    // Last, because the list is applied in order and these land inside the
    // worktree or the home: anything laid over them afterwards covers them.
    mounts.extend(declared.iter().map(declared_bind));
    mounts
}

/// A host path the sandbox carries. Read-only stays denied for the life of the
/// box: the sandbox holds no capability to remount anything.
fn declared_bind(mount: &SandboxMount) -> Mount {
    let mut bind = Mount::default();
    bind.set_destination(mount.target.clone());
    bind.set_typ(Some("bind".to_string()));
    bind.set_source(Some(mount.source.clone()));
    let access = match mount.access {
        MountAccess::ReadOnly => "ro",
        MountAccess::ReadWrite => "rw",
    };
    bind.set_options(Some(vec!["rbind".to_string(), access.to_string()]));
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
        .set_seccomp(Some(sandbox_seccomp()))
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

/// The syscall filter every sandbox runs under, vendored verbatim from the
/// containers project and carried inside the binary. `assets/seccomp/PROVENANCE`
/// names the revision it came from.
const SECCOMP_PROFILE: &str = include_str!("../../assets/seccomp/default.json");

/// The two names one host answers to inside the profile: the token the rules
/// gate on, and the entry `archMap` is keyed by.
struct HostArchitecture {
    profile: &'static str,
    seccomp: Arch,
}

// A host named nowhere here cannot be resolved against the profile at all, and
// the compile error at the first use is how it says so.
#[cfg(target_arch = "x86_64")]
const HOST_ARCHITECTURE: HostArchitecture =
    HostArchitecture { profile: "amd64", seccomp: Arch::ScmpArchX86_64 };
#[cfg(target_arch = "aarch64")]
const HOST_ARCHITECTURE: HostArchitecture =
    HostArchitecture { profile: "arm64", seccomp: Arch::ScmpArchAarch64 };

/// The profile as it is written, which is not the shape the spec types describe.
/// It extends them with a top level `archMap` and with a condition on every
/// rule, naming the container each one is written for, and those types accept
/// fields they do not know without complaint. So handing the file straight to
/// them succeeds and drops the conditions rather than answering them, which is
/// most of what the file means: for a container holding no capability, which is
/// every sandbox hort builds, every rule written for a holder would arrive as a
/// plain allow, `bpf` and `chroot` and unrestricted `socket` among them.
/// Resolving the conditions is what the file is for, not a refinement of it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeccompProfile {
    default_action: LinuxSeccompAction,
    default_errno_ret: Option<u32>,
    arch_map: Vec<ArchitectureFamily>,
    syscalls: Vec<ProfileRule>,
}

/// One architecture together with the ones a machine of that architecture also
/// runs binaries of.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchitectureFamily {
    architecture: Arch,
    sub_architectures: Vec<Arch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileRule {
    names: Vec<String>,
    action: LinuxSeccompAction,
    errno_ret: Option<u32>,
    args: Option<Vec<LinuxSeccompArg>>,
    #[serde(default)]
    includes: RuleCondition,
    #[serde(default)]
    excludes: RuleCondition,
}

/// What a rule asks of the container it is written for.
#[derive(Default, Deserialize)]
struct RuleCondition {
    #[serde(default)]
    arches: Vec<String>,
    #[serde(default)]
    caps: Vec<String>,
}

/// The filter the sandbox runs under: the vendored profile with every condition
/// in it answered for this host.
fn sandbox_seccomp() -> LinuxSeccomp {
    let profile: SeccompProfile = serde_json::from_str(SECCOMP_PROFILE)
        .expect("the profile compiled into hort is the one vendored beside its provenance");
    resolved_profile(&profile)
}

fn resolved_profile(profile: &SeccompProfile) -> LinuxSeccomp {
    let rules: Vec<LinuxSyscall> = profile
        .syscalls
        .iter()
        .filter(|rule| written_for_the_sandbox(rule))
        .map(resolved_rule)
        .collect();
    let mut seccomp = LinuxSeccomp::default();
    seccomp
        .set_default_action(profile.default_action)
        .set_default_errno_ret(profile.default_errno_ret)
        .set_architectures(resolved_architectures(profile))
        .set_syscalls(Some(rules));
    seccomp
}

/// The architectures beyond the native one the filter covers. Nothing in this
/// repo can tell this apart from naming none, because the difference only shows
/// in a process of another architecture and this machine runs none: left out,
/// the calls a 32-bit binary in the sandbox makes match no rule and fall to the
/// default action, which refuses them, so the binary does not run at all. It
/// stays because carrying the profile verbatim is worth nothing if what hort
/// installs quietly means something narrower than what upstream wrote. A profile
/// that names no family for this host is naming no second architecture to cover,
/// so nothing there is an answer rather than a failure.
fn resolved_architectures(profile: &SeccompProfile) -> Option<Vec<Arch>> {
    let family =
        profile.arch_map.iter().find(|family| family.architecture == HOST_ARCHITECTURE.seccomp)?;
    let mut architectures = vec![family.architecture];
    architectures.extend(family.sub_architectures.iter().copied());
    Some(architectures)
}

/// Whether one rule of the profile is written for the container hort builds. The
/// two conditions look symmetrical and are opposites: `includes` names what that
/// container must have for the rule to apply, `excludes` what it must not.
fn written_for_the_sandbox(rule: &ProfileRule) -> bool {
    // A sandbox holds no capability at all, which collapses both capability
    // halves into this one question. Grant one some day and undo this first.
    if !rule.includes.caps.is_empty() {
        return false;
    }
    if names_this_host(&rule.excludes.arches) {
        return false;
    }
    rule.includes.arches.is_empty() || names_this_host(&rule.includes.arches)
}

fn names_this_host(arches: &[String]) -> bool {
    arches.iter().any(|arch| arch == HOST_ARCHITECTURE.profile)
}

/// The rule with its conditions spent, which is the shape the spec describes.
fn resolved_rule(rule: &ProfileRule) -> LinuxSyscall {
    let mut syscall = LinuxSyscall::default();
    syscall
        .set_names(rule.names.clone())
        .set_action(rule.action)
        .set_errno_ret(rule.errno_ret)
        .set_args(rule.args.clone());
    syscall
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

    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    use libcontainer::container::ContainerStatus;
    use libcontainer::oci_spec::runtime::{
        Capabilities, LinuxIdMappingBuilder, LinuxNamespaceType, LinuxSeccomp, LinuxSeccompAction,
    };

    use crate::adapters::liveness::ProcLivenessProbe;
    use crate::ports::{LivenessProbe, MountAccess, ResourceLimits, SandboxMount};

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
            drop_ins: Vec::new(),
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
                access: MountAccess::ReadOnly,
            }],
            ..sandbox_spec()
        }
    }

    /// A sandbox carrying both families at once: the user's own configuration,
    /// which it may only read, and two caches of the project it is a sandbox of,
    /// one addressed inside the worktree and one inside the home.
    fn spec_mounting_a_dotfile_and_two_caches() -> OciSpec {
        OciSpec {
            mounts: vec![
                SandboxMount {
                    source: PathBuf::from("/home/tester/.config/fish"),
                    target: PathBuf::from("/home/hort/.config/fish"),
                    access: MountAccess::ReadOnly,
                },
                SandboxMount {
                    source: PathBuf::from("/state/cache/%2Fproject/node_modules"),
                    target: PathBuf::from("/workdir/node_modules"),
                    access: MountAccess::ReadWrite,
                },
                SandboxMount {
                    source: PathBuf::from("/state/cache/%2Fproject/pip"),
                    target: PathBuf::from("/home/hort/.cache/pip"),
                    access: MountAccess::ReadWrite,
                },
            ],
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
    fn spec_mounts_a_cache_path_writable() {
        let assembled = anchor_spec(&spec_mounting_a_dotfile_and_two_caches());

        // A cache exists to be filled from inside the box: read-only it holds
        // whatever the first run put there and refuses every install after it,
        // which is worse than having no cache at all, because the box now fails
        // where a box without one merely waited.
        let mounts = assembled.mounts().as_ref().unwrap();
        let cache = &mounts[mount_index(&assembled, "/workdir/node_modules")];
        assert_eq!(cache.source(), &Some(PathBuf::from("/state/cache/%2Fproject/node_modules")));
        assert!(!cache.options().as_ref().unwrap().contains(&"ro".to_string()));
    }

    #[test]
    fn declared_mounts_come_after_every_tmpfs_that_would_hide_them() {
        let assembled = anchor_spec(&spec_mounting_a_dotfile_and_two_caches());

        // The mount list is applied in order, so a tmpfs laid down after a bind
        // that lands underneath it covers that bind: the box comes up missing
        // what the user declared, no error anywhere, and a shell that silently
        // behaves like nobody's. Which family the bind came from decides
        // nothing: a cache addressed inside the home is swallowed exactly like a
        // dotfile is, and a declared source outside the user's own home keeps
        // its absolute path and can land under any of them.
        let last_tmpfs = last_tmpfs_index(&assembled);
        assert!(last_tmpfs < mount_index(&assembled, "/home/hort/.config/fish"));
        assert!(last_tmpfs < mount_index(&assembled, "/home/hort/.cache/pip"));
    }

    #[test]
    fn a_cache_bind_comes_after_the_worktree_that_holds_it() {
        let assembled = anchor_spec(&spec_mounting_a_dotfile_and_two_caches());

        // The other mount that swallows one, and a different one: a cache named
        // bare is addressed inside the worktree, so the bind that carries the
        // worktree covers it if it lands second. What the box would see at that
        // path is then the project's own empty directory, and every install
        // would go back to writing into the sandbox it dies with.
        assert!(
            mount_index(&assembled, "/workdir") < mount_index(&assembled, "/workdir/node_modules")
        );
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

    /// A syscall the profile allows only on some machines, named for the one
    /// these tests are built for. There is no portable name to put here: a group
    /// takes that form precisely because it means nothing on another
    /// architecture. A host this does not name has to be added both here and
    /// wherever the profile resolves architectures, and the compile error is how
    /// it says so.
    #[cfg(target_arch = "x86_64")]
    const GATED_ON_THIS_ARCHITECTURE: &str = "arch_prctl";
    #[cfg(target_arch = "aarch64")]
    const GATED_ON_THIS_ARCHITECTURE: &str = "set_tls";

    fn seccomp_of(assembled: &Spec) -> &LinuxSeccomp {
        assembled
            .linux()
            .as_ref()
            .unwrap()
            .seccomp()
            .as_ref()
            .expect("the assembled spec carries a seccomp profile")
    }

    /// Every action the assembled profile carries for `syscall`, once per rule
    /// naming it. A syscall listed twice appears twice, which is what the
    /// question needs: one allowing rule is enough to allow it, whatever is
    /// written beside it.
    fn actions_for(assembled: &Spec, syscall: &str) -> Vec<LinuxSeccompAction> {
        seccomp_of(assembled)
            .syscalls()
            .as_ref()
            .expect("the profile carries syscall rules")
            .iter()
            .filter(|rule| rule.names().iter().any(|name| name == syscall))
            .map(|rule| rule.action())
            .collect()
    }

    #[test]
    fn spec_denies_a_syscall_the_profile_gates_on_a_capability_the_sandbox_lacks() {
        let assembled = anchor_spec(&sandbox_spec());

        // The profile is written for a container that may hold capabilities and
        // a sandbox holds none, so every group written for a holder has to go.
        // Keeping one costs nothing a reader would notice: with the condition
        // gone it is an ordinary allow rule, and the refusing twin written
        // beside it is dropped for repeating the default action. This call is
        // the one to ask about because it opens a file by handle instead of by
        // path, and path resolution is the whole of what a mount namespace
        // confines. The default action is asserted with it because a rule list
        // that never allows a call says nothing about that call unless what goes
        // unlisted is refused.
        assert_eq!(seccomp_of(&assembled).default_action(), LinuxSeccompAction::ScmpActErrno);
        assert!(
            !actions_for(&assembled, "open_by_handle_at")
                .contains(&LinuxSeccompAction::ScmpActAllow)
        );
    }

    #[test]
    fn spec_allows_a_syscall_the_profile_gates_on_the_host_architecture() {
        let assembled = anchor_spec(&sandbox_spec());

        // Dropping every conditional group is the tempting way to be safe and it
        // shuts the box instead of closing it: this group carries the call the C
        // library makes to set up thread-local storage, and without it nothing
        // in the sandbox reaches its first instruction.
        assert!(
            actions_for(&assembled, GATED_ON_THIS_ARCHITECTURE)
                .contains(&LinuxSeccompAction::ScmpActAllow)
        );
    }

    #[test]
    fn spec_allows_a_syscall_the_profile_gates_on_lacking_a_capability() {
        let assembled = anchor_spec(&sandbox_spec());

        // The profile states the socket rule twice, unrestricted for a container
        // allowed to write audit records and restricted for one that is not, and
        // a sandbox is the second. The two conditions look symmetrical and are
        // opposites: read this one the way the one above it reads, and the box
        // is left unable to open a socket at all, which is every agent in it
        // unable to reach anything.
        assert!(actions_for(&assembled, "socket").contains(&LinuxSeccompAction::ScmpActAllow));
    }

    #[test]
    fn spec_sets_no_new_privileges() {
        let assembled = anchor_spec(&sandbox_spec());

        // Loading a filter takes either this bit or admin rights inside the user
        // namespace, and the runtime reads the field to decide when to load: set,
        // it raises the bit early and loads next to the exec; absent, it loads
        // while capabilities are still held; set to false it does neither and
        // the load fails outright.
        let process = assembled.process().as_ref().unwrap();
        assert_eq!(process.no_new_privileges(), Some(true));
    }

    #[test]
    fn session_process_sets_no_new_privileges() {
        let spec = SessionSpec {
            name: SandboxName::new("demo").unwrap(),
            command: vec!["/bin/sh".to_string()],
            cwd: PathBuf::from("/workdir"),
            env: Vec::new(),
            terminal: true,
        };

        // A session that asks for a terminal is handed a process file, and the
        // file replaces the process the tenant API would have built rather than
        // adding to it, taking with it the field that API would have copied off
        // the sandbox. Set to false, the runtime never raises the bit and a
        // filter load fails outright; left out, nothing raises it at all, which
        // is a setuid binary in the box free to gain what the empty capability
        // set is there to deny.
        let process = session_process(&spec);
        assert_eq!(process.no_new_privileges(), Some(true));
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

    #[test]
    fn the_anchor_is_not_among_the_sessions_of_its_own_sandbox() {
        let anchor = 4242;

        let sessions = sessions_among(vec![anchor, 5150], anchor);

        // The anchor sits in the sandbox's cgroup like everything else in the
        // box, and the three callers count what comes out of here without
        // looking at it again. Handing the read through unchanged makes an empty
        // sandbox report one session, which asks before every `down` and leaves
        // an idle box permanently active.
        assert_eq!(sessions, vec![5150]);
    }

    #[test]
    fn an_anchor_missing_from_the_process_list_leaves_the_rest_reported() {
        let sessions = sessions_among(vec![5150], 4242);

        // The anchor pid comes from the container state and the process list
        // from the kernel, so the two are read a moment apart and an anchor that
        // died in between is simply not in the list. Removing it by position, or
        // insisting it be there, turns that ordinary race into a failure and
        // loses the sessions that outlived it.
        assert_eq!(sessions, vec![5150]);
    }

    #[test]
    fn has_container_state_is_true_while_the_container_directory_stands() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = SandboxName::new("demo").unwrap();
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(a_live_pid()),
        );
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        // A read that answered absence for every sandbox would have `attach`
        // refuse every live box on the machine with a message about lost
        // state, which is why the standing case is pinned and not only the
        // vanished one.
        assert!(runtime.has_container_state(&name));
    }

    #[test]
    fn has_container_state_is_false_once_the_container_directory_vanished() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = SandboxName::new("demo").unwrap();
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(a_live_pid()),
        );
        // This one container's directory and nothing above it, which is what a
        // hand removal leaves: the parent still stands, so a read that looked
        // for the parent alone would say the state is there.
        fs::remove_dir_all(runtime_root.path().join("containers").join(name.as_str())).unwrap();
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        assert!(!runtime.has_container_state(&name));
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
        let name = a_name_nothing_else_answers_to("recorded");
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(a_live_pid()),
        );
        let registry = registry_over(runtime_root.path());

        let live = registry.list_live().unwrap();

        // Nothing else in hort answers "which anchors are up". A registry that
        // finds none reports every sandbox in `ls` as orphaned while its anchor
        // runs, and hands `prune` a live box as debris to remove.
        assert!(live.iter().any(|entry| entry.id == name));
    }

    #[test]
    fn registry_reports_a_token_the_liveness_probe_recognizes() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("recognizable");
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(a_live_pid()),
        );
        let registry = registry_over(runtime_root.path());

        let live = registry.list_live().unwrap();

        let reported = live.iter().find(|entry| entry.id == name).expect("the recorded sandbox");
        // Reconciliation matches this token against the one the record carries,
        // and that one was read the way this probe reads it. A registry that
        // reports the right sandbox under a token nobody recognizes is the same
        // outcome as reporting nothing, and looks correct from every other angle.
        assert!(ProcLivenessProbe.is_alive(&reported.token));
    }

    #[test]
    fn registry_skips_a_container_directory_it_cannot_read() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("beside-a-half-written-one");
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(a_live_pid()),
        );
        fs::create_dir_all(runtime_root.path().join("containers").join("half-written")).unwrap();

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The registry reads state another process wrote and the kernel may have
        // outlived, so a directory it cannot make sense of is an ordinary state,
        // one an interrupted build or a container removed mid-walk leaves behind.
        // Failing on it would take down every command that reconciles, and the
        // only way out would be deleting files by hand.
        assert!(live.iter().any(|entry| entry.id == name));
    }

    #[test]
    fn registry_omits_a_container_whose_anchor_is_not_running() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("recorded-short-of-running");
        // A live pid with a status short of running: the runtime keeps that
        // status rather than promoting it, so nothing but the status itself
        // stands between this entry and being read as a live anchor. Recording
        // it with a dead pid instead would prove nothing, because a pid nobody
        // can read is dropped a step later anyway.
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Created,
            Some(a_live_pid()),
        );

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The container state outlives the anchor it describes, so the directory
        // alone means nothing. Reading it as alive makes `ls` report a dead
        // sandbox as live and keeps `prune` from ever offering to clean it.
        assert!(!live.iter().any(|entry| entry.id == name));
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
    fn registry_answers_a_root_that_was_never_built_rather_than_refusing_it() {
        let runtime_root = tempfile::tempdir().unwrap();
        let registry = LibcontainerRuntime::new(runtime_root.path().to_path_buf());

        let live = registry.list_live();

        // Nothing has run yet: the root is created by the first build, and every
        // command that reconciles asks this before then. What comes back is not
        // the point, since a root nothing was built under is no evidence about
        // what is running; that an answer comes back at all is, and it is the
        // honest counterpart of refusing a root that cannot be read.
        assert!(live.is_ok());
    }

    /// A sandbox name nothing else on this machine answers to. These tests read
    /// the real process table, so a name shared with anything else running would
    /// have them assert about somebody else's process.
    fn a_name_nothing_else_answers_to(suffix: &str) -> SandboxName {
        SandboxName::new(&format!("scan-{}-{suffix}", std::process::id())).unwrap()
    }

    /// A real process of this user, alive until the test lets go of it and taken
    /// away whether that test reached its own end or died on an assertion.
    ///
    /// It announces itself before anything reads it because spawning comes back
    /// before the exec does: measured 464 times in 500, the pid already existed
    /// while the environment behind it still belonged to the process that started
    /// it, so a read taken straight after spawning reads the test harness.
    ///
    /// The announcement comes from the program the process keeps, and that is
    /// what makes it an answer rather than a hint. A process that swaps programs
    /// once more after announcing opens a second window the signal says nothing
    /// about, and the kernel does not report that window as a failure: while it
    /// builds the environment of the incoming program, a read of that process's
    /// environment succeeds and comes back empty. A scan landing there finds a
    /// process declaring no sandbox rather than one it could not read, which is
    /// indistinguishable from the defect these tests exist to catch. Measured at
    /// the moment of failure, that empty read is what made this fixture fail its
    /// own witness about once in thirty passes.
    ///
    /// Nothing but this handle keeps it alive: it blocks on a pipe this process
    /// holds open, so a harness that dies with no destructor run still takes it
    /// along. Once hort reads the process table, one of these left behind is a
    /// sandbox it reports.
    struct HostProcess(Child);

    impl HostProcess {
        fn declaring(sandbox: &SandboxName) -> Self {
            Self::announcing(Command::new("sh").env("HORT_SANDBOX", sandbox.as_str()))
        }

        /// A process carrying a variable whose name merely begins the way the
        /// marker does, and no marker. The removal is not belt and braces: it is
        /// what keeps this a process that declares nothing when the suite is run
        /// from inside a sandbox.
        fn under_a_lookalike_variable(sandbox: &SandboxName) -> Self {
            Self::announcing(
                Command::new("sh")
                    .env("HORT_SANDBOX_OLD", sandbox.as_str())
                    .env_remove("HORT_SANDBOX"),
            )
        }

        fn announcing(command: &mut Command) -> Self {
            let mut child = command
                .args(["-c", "echo ready; read held_open"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut announcement = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut announcement).unwrap();
            Self(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// Whether the process is still running, asked once anything sent its way
        /// has had time to land.
        ///
        /// A signal is delivered to a running process and acted on when that
        /// process next runs, so one asked about the instant after a kill was
        /// sent can still answer that it is alive while already being doomed.
        /// Measured on this same kind of process, a kill leaves it reapable
        /// within a tenth of a millisecond, and the wait here is three orders of
        /// magnitude of that.
        fn is_still_running(&mut self) -> bool {
            sleep(Duration::from_millis(100));
            self.0.try_wait().unwrap().is_none()
        }
    }

    impl Drop for HostProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn registry_reports_a_live_anchor_whose_container_state_is_gone() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("vanished");
        let anchor = HostProcess::declaring(&name);

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The container state is bookkeeping the anchor does not depend on: an
        // older build wrote it where this one does not look, and a hand that
        // removed it loses it outright. Answering only from those files calls a
        // sandbox dead while its anchor runs, which is what lets `prune` select
        // it and reach the step that deletes a worktree the container still
        // holds mounted.
        assert!(live.iter().any(|entry| entry.id == name && entry.token.pid.0 == anchor.pid()));
    }

    #[test]
    fn an_anchor_both_sources_know_about_is_reported_once() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("known-twice");
        let anchor = HostProcess::declaring(&name);
        record_container(
            runtime_root.path(),
            name.as_str(),
            ContainerStatus::Running,
            Some(anchor.pid() as i32),
        );

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // The two sources overlap for every healthy sandbox, so an enumeration
        // that simply concatenates them reports the ordinary case twice. A
        // sandbox with no record then earns a lost-record row per source, and
        // whoever reads that listing cannot tell one box from two.
        assert_eq!(live.iter().filter(|entry| entry.id == name).count(), 1);
    }

    #[test]
    fn a_process_of_this_user_declaring_no_sandbox_is_not_an_anchor() {
        let runtime_root = tempfile::tempdir().unwrap();
        // The variable only looks like the marker, and it is worth what it costs:
        // a process carrying nothing at all cannot tell a careful reading from a
        // careless one, since a careless one has no name to build an entry out
        // of. This one hands it a name, and a name that is valid on purpose, or
        // the mistake would be caught by the name and not by this test.
        let name = a_name_nothing_else_answers_to("lookalike");
        let stranger = HostProcess::under_a_lookalike_variable(&name);

        let live = registry_over(runtime_root.path()).list_live().unwrap();

        // Everything this user runs is readable exactly the way an anchor is, so
        // what a process says about itself is the whole of what separates a
        // sandbox from the rest of the login session. Without that line drawn,
        // `ls` fills with lost records nobody made and `prune` offers to clean
        // processes that were never hort's to touch.
        assert!(!live.iter().any(|entry| entry.token.pid.0 == stranger.pid()));
    }

    #[test]
    fn teardown_does_not_stop_a_host_process_that_merely_declares_the_sandbox() {
        let runtime_root = tempfile::tempdir().unwrap();
        let name = a_name_nothing_else_answers_to("impostor");
        let mut impostor = HostProcess::declaring(&name);
        let runtime = LibcontainerRuntime::new(runtime_root.path().to_path_buf());
        // What follows is only worth reading if this process is one hort sees
        // under that name: a process the enumeration never finds is one nothing
        // could have signalled by mistake.
        let live = runtime.list_live().unwrap();
        assert!(
            live.iter().any(|entry| entry.id == name && entry.token.pid.0 == impostor.pid()),
            "the process is not enumerated under the sandbox it declares"
        );

        runtime.teardown(&name).unwrap();

        // Exporting a variable is not what makes a sandbox, and anything this
        // user runs is free to export it, the shell hort was typed into
        // included. A teardown that signals on the strength of that name alone
        // reaches whatever the host happens to be running under it.
        //
        // Which is also the warning owed to whoever loosens that check to try
        // something: on a tree where it is gone, this test signals the processes
        // sharing its own cgroup, and on a developer's machine that is the shell
        // and whatever is driving it. Run it under a scope of its own before
        // reverting anything here.
        assert!(impostor.is_still_running(), "the teardown stopped a process outside any sandbox");
    }

    #[test]
    fn a_drop_in_lands_in_the_writable_layer_at_its_container_path() {
        let overlay = tempfile::tempdir().unwrap();
        let settings = SandboxFile {
            path: PathBuf::from("/etc/claude-code/managed-settings.d/hort-notify.json"),
            content: "{\"hooks\":{}}".to_string(),
        };

        write_drop_ins(overlay.path(), std::slice::from_ref(&settings)).unwrap();

        // A container path is absolute, and joining an absolute path onto another
        // discards the first: written that way the file lands on the host's own
        // /etc, where it either fails for want of permission or, on a machine
        // that grants it, edits the settings of the person running hort. Neither
        // needs a kernel to catch, which is why this is asked here rather than
        // from inside a real box. The parents come with it because none of them
        // exists in any prepared rootfs.
        let landed = overlay
            .path()
            .join("upper")
            .join("etc/claude-code/managed-settings.d/hort-notify.json");
        assert_eq!(fs::read_to_string(landed).ok().as_deref(), Some("{\"hooks\":{}}"));
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

    use crate::adapters::environment::HostEnvironmentProbe;
    use crate::adapters::gated::ScratchSandbox;
    use crate::adapters::helper::a_declared_port;
    use crate::adapters::proxy;
    use crate::adapters::streams::sandbox_log_path;
    use crate::domain::egress::{EgressPolicy, HostPattern};
    use crate::domain::model::{BranchName, Domain, SandboxRecord};
    use crate::domain::reconcile::{SandboxState, reconcile_all};
    use crate::ports::{
        DbForward, EnvironmentProbe, MountAccess, NetworkProvider, NetworkSpec, SandboxMount,
        Worktree,
    };

    const ANCHOR_DEADLINE: Duration = Duration::from_secs(5);
    /// How long a session is given to exec and dial, and therefore also how long
    /// a connection is waited for before its absence counts as absence.
    const SESSION_DEADLINE: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(50);
    /// The file a sandbox records the ports its sessions may connect to in.
    const CONNECT_PORTS_FILE: &str = "connect.ports";
    /// The file the embedded runtime keeps a container's cgroup path in, next to
    /// the rest of that container's state.
    const RUNTIME_CGROUP_RECORD: &str = "youki_config.json";
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

    /// A session carrying what `attach` states about the sandbox it enters, and
    /// staying alive long enough to be read from the host.
    ///
    /// Every session hort opens carries this, so the process table holds one
    /// process per session naming the sandbox on top of the anchor naming it.
    fn declaring_session(spec: &OciSpec) -> SessionSpec {
        SessionSpec {
            name: spec.name.clone(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            cwd: PathBuf::from(WORKDIR),
            env: vec![
                ("HORT_SANDBOX".to_string(), spec.name.as_str().to_string()),
                ("HORT_WORKTREE".to_string(), spec.workdir.display().to_string()),
            ],
            terminal: false,
        }
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

    /// The file a session leaves in `/workdir` once what a stop request does to
    /// it is its own to answer for.
    ///
    /// A session exists as a pid before it execs, and until it does, a signal
    /// reaches whatever hort started it as rather than what it was written to
    /// become. Without this, a request that arrives early kills a session that
    /// was never arranged to survive one.
    const READY_FOR_A_STOP: &str = "ready-for-a-stop";
    /// The file a session leaves in `/workdir` when it is asked to stop instead
    /// of being killed where it stands.
    const ASKED_TO_STOP: &str = "asked-to-stop";

    /// A session nothing short of a kill takes down, which is what an agent that
    /// holds off a stop request until it has finished writing looks like from
    /// outside the box.
    ///
    /// One process and not two: a signal set to be ignored stays ignored across
    /// an exec while a handler does not, so the `sleep` this becomes carries the
    /// disposition, and the session holds no child that could die of the request
    /// and end it that way instead.
    fn stop_ignoring_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("trap '' TERM; touch {WORKDIR}/{READY_FOR_A_STOP}; exec sleep infinity"),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
        }
    }

    /// A session that records having been asked to stop, and then stops.
    ///
    /// It waits on a background child rather than running a foreground one
    /// because a shell holds a pending handler until the command it is waiting
    /// on returns, and the wait built into it is what returns on the signal
    /// itself.
    fn stop_reporting_session(name: &SandboxName) -> SessionSpec {
        SessionSpec {
            name: name.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "trap 'touch {WORKDIR}/{ASKED_TO_STOP}; exit 0' TERM; touch {WORKDIR}/{READY_FOR_A_STOP}; sleep infinity & wait"
                ),
            ],
            cwd: PathBuf::from(WORKDIR),
            env: Vec::new(),
            terminal: false,
        }
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
            resolver: None,
        }
    }

    fn open_network(name: &SandboxName, anchor: u32) -> NetworkSpec {
        NetworkSpec {
            name: name.clone(),
            netns: PathBuf::from(format!("/proc/{anchor}/ns/net")),
            egress: EgressPolicy::Open,
            db_forwards: Vec::new(),
            resolver: None,
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
        SandboxMount {
            source,
            target: PathBuf::from(MOUNTED_DOTFILE_DIR),
            access: MountAccess::ReadOnly,
        }
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

    /// Wait until the session is the process the sandbox opened it as, which is
    /// when its environment first names the sandbox.
    ///
    /// A session exists as a pid before it execs, and until then the environment
    /// behind that pid is still the one hort was invoked with, which names no
    /// sandbox. Read that early, a session is indistinguishable from any other
    /// process on the host, and a test asking whether the scan tells it from an
    /// anchor would be asking about a process the scan never had to judge. The
    /// question is production's own, so what counts as naming the sandbox cannot
    /// drift from what the scan counts.
    fn wait_for_session_to_declare(pid: u32, name: &SandboxName) {
        let deadline = Instant::now() + SESSION_DEADLINE;
        while Instant::now() < deadline {
            let declared = fs::read(format!("/proc/{pid}/environ"))
                .ok()
                .and_then(|environ| declared_sandbox(&environ));
            if declared.as_ref() == Some(name) {
                return;
            }
            sleep(POLL);
        }
        panic!("the session never named '{}' within {SESSION_DEADLINE:?}", name.as_str());
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

        let token = runtime.start_anchor(&spec).unwrap();

        let anchor_mnt_ns = fs::metadata(format!("/proc/{}/ns/mnt", token.pid.0)).unwrap();
        assert_eq!(token.mnt_ns.0, anchor_mnt_ns.ino());
        runtime.teardown(&spec.name).unwrap();
    }

    /// The record `up` would have persisted for this sandbox, short of the token
    /// its caller stamps on once the anchor is running.
    fn record_of(spec: &OciSpec) -> SandboxRecord {
        SandboxRecord::new(
            spec.name.clone(),
            Some(BranchName::new(spec.name.as_str()).unwrap()),
            spec.workdir.clone(),
            spec.overlay.clone(),
            "2026-08-20T12:00:00Z".to_string(),
            "2026-08-20T12:00:00Z".to_string(),
            None,
            spec.workdir.clone(),
        )
    }

    /// A sandbox's container state moved out of the directory the enumeration
    /// walks, and put back however the test that took it away ended.
    ///
    /// With those files gone the runtime has no handle to stop the container
    /// through and falls back to signalling what the kernel still has of it, so
    /// the anchor comes down either way. What putting them back decides is which
    /// arm the scratch sandbox's own teardown runs: restored, it runs the
    /// runtime's delete over the state it wrote, the way every other test's
    /// does, rather than the fallback under test a second time. Declare it after
    /// the sandbox, so it goes first.
    struct ContainerStateAside {
        home: PathBuf,
        aside: PathBuf,
    }

    impl ContainerStateAside {
        fn taken_from(sandbox: &ScratchSandbox, name: &SandboxName) -> Self {
            let home = sandbox.runtime_root().join(CONTAINERS_DIR).join(name.as_str());
            // Out of the walked directory rather than renamed inside it, which is
            // also where the build that produced the first of these left it.
            let aside = sandbox.runtime_root().join(name.as_str());
            fs::rename(&home, &aside).unwrap();
            Self { home, aside }
        }
    }

    impl Drop for ContainerStateAside {
        fn drop(&mut self) {
            let _ = fs::rename(&self.aside, &self.home);
        }
    }

    /// The container's own record of where its cgroup lives, pointed at one
    /// nothing on this host answers to, and put back however the test that moved
    /// it ended.
    ///
    /// It buys the one arrangement the race itself cannot produce. A cgroup the
    /// runtime cannot read arises on its own only once the anchor is dead, since
    /// what takes the cgroup away is the anchor leaving it, so an anchor that is
    /// still up and a cgroup that is already gone never meet by accident.
    ///
    /// Putting it back is not tidiness. Left pointing away, the scratch
    /// sandbox's own teardown fails exactly as the one under test did, and a
    /// real anchor is left running on the machine. Declare it after the sandbox,
    /// so it goes first.
    struct CgroupPathAside {
        record: PathBuf,
        recorded: String,
    }

    impl CgroupPathAside {
        fn pointed_away_from(sandbox: &ScratchSandbox, name: &SandboxName) -> Self {
            let record = sandbox
                .runtime_root()
                .join(CONTAINERS_DIR)
                .join(name.as_str())
                .join(RUNTIME_CGROUP_RECORD);
            let recorded = fs::read_to_string(&record).unwrap();
            let elsewhere =
                recorded.replace(name.as_str(), &format!("{}-elsewhere", name.as_str()));
            assert_ne!(elsewhere, recorded, "the cgroup record names no sandbox to move");
            fs::write(&record, elsewhere).unwrap();
            Self { record, recorded }
        }
    }

    impl Drop for CgroupPathAside {
        fn drop(&mut self) {
            let _ = fs::write(&self.record, &self.recorded);
        }
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_record_whose_container_state_vanished_still_reconciles_live() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let record = record_of(&spec).with_token(token);
        let worktrees = vec![Worktree { path: spec.workdir.clone() }];
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        let verdicts = reconcile_all(&[record], &runtime.list_live().unwrap(), &worktrees);

        // Only a real box can say that what hort writes into a sandbox's
        // environment is still readable from the host once the anchor is up, and
        // with the container state out of reach that marker is the whole of what
        // is left to recognize the anchor by. Everywhere else this is inference
        // off the spec the container was built from. Read wrong, the record
        // reconciles orphaned while the container still holds the worktree
        // mounted, and `prune` takes that worktree away next.
        assert!(verdicts.contains(&(spec.name.clone(), SandboxState::Live)));
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn registry_reports_the_token_the_anchor_was_started_under() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let started = runtime.start_anchor(&spec).unwrap();

        let live = runtime.list_live().unwrap();

        // The two halves of reconciliation are written by different code at
        // different times: `up` records this token, and this read produces the
        // one it is matched against. Only a real anchor can say they agree, since
        // only here does the pid come from the runtime's own state file rather
        // than from the test. Whole-token equality is the contract: same pid
        // under an inode read another way still reads as a dead sandbox.
        assert!(live.iter().any(|entry| entry.id == spec.name && entry.token == started));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn registry_stops_reporting_a_sandbox_whose_anchor_was_killed() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let started = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(started.pid.0);
        unsafe { libc::kill(started.pid.0 as libc::pid_t, libc::SIGKILL) };
        assert!(stopped_within_deadline(started.pid.0), "the anchor outlived the kill");

        let live = runtime.list_live().unwrap();

        // The container state survives the anchor, and hort is built to reconcile
        // against the kernel rather than to prevent the kill. A registry reading
        // the leftover state as a live anchor is what would make `ls` insist a
        // killed sandbox is running and keep `prune` from clearing the debris.
        assert!(!live.iter().any(|entry| entry.id == spec.name));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_joined_to_a_sandbox_is_not_read_as_its_anchor() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let session = runtime.join_session(&declaring_session(&spec)).unwrap();
        wait_for_session_to_declare(session.pid, &spec.name);

        let declared = declared_anchors();

        // A session names the same sandbox as the anchor it joined and lands in
        // the same mount namespace, so the token read off it differs from the
        // anchor's in the pid alone, while a record is matched against the whole
        // token. Taken for the anchor, a session makes a running sandbox
        // reconcile as orphaned, which is the outcome this scan exists to
        // prevent: `prune` then selects the box and deletes the worktree the
        // container is still holding mounted. Membership rather than position,
        // because the walk takes pids in ascending order and an anchor is
        // started before any session of its own, so asking which of them comes
        // first is a question the host answers the same way either way.
        assert!(!declared.iter().any(|entry| entry.token.pid.0 == session.pid));
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_runs_with_an_empty_capability_set() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

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

    /// The `Seccomp` line of a process the kernel has loaded a filter for. A
    /// process running under none reads `0` on that line.
    const FILTER_LOADED: &str = "Seccomp:\t2";

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_runs_under_a_seccomp_filter() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

        let token = runtime.start_anchor(&spec).unwrap();

        wait_for_anchor(token.pid.0);
        let status = fs::read_to_string(format!("/proc/{}/status", token.pid.0)).unwrap();
        assert!(status.contains(FILTER_LOADED), "the anchor runs with no filter loaded");
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_on_a_terminal_runs_under_a_seccomp_filter() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        // Held while the answer is read: closing the master hangs the session's
        // terminal up, and the wait is what keeps the read off a session that
        // exists as a pid and has not become what it was opened as yet.
        let session = runtime.join_session(&reporting_session(&spec.name)).unwrap();
        assert!(
            appeared_within_deadline(&spec.workdir.join(TERMINAL_WITNESS)),
            "the session never reached the command it was opened for"
        );

        // Nothing a session is given carries the filter to it on its own. The
        // tenant path rebuilds the container's linux section from the namespaces
        // it found plus a handful of fields, and the seccomp profile is not one
        // of them, whether the session was described by a process file or by the
        // builder's own setters. This is the shell a person types into and the
        // one an agent runs under, so a filter the anchor carries and this does
        // not is a filter nothing that matters runs under.
        let status = fs::read_to_string(format!("/proc/{}/status", session.pid)).unwrap();
        assert!(status.contains(FILTER_LOADED), "the session runs with no filter loaded");
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_sees_the_worktree_at_workdir() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);

        runtime.teardown(&spec.name).unwrap();

        assert!(stopped_within_deadline(token.pid.0));
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_stops_an_anchor_whose_container_state_is_gone() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        runtime.teardown(&spec.name).unwrap();

        // Those files are bookkeeping the anchor does not depend on: losing them
        // costs the runtime the only handle it had on the container and costs the
        // container nothing at all. Read as a box that is already down, the loss
        // buys a successful teardown over a sandbox still holding the worktree
        // mounted, and whoever asked for the teardown deletes that worktree next.
        assert!(stopped_within_deadline(token.pid.0), "the anchor outlived the teardown");
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_stops_a_session_of_a_sandbox_whose_container_state_is_gone() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        // Joined while the state is still there, because joining reads the very
        // files the arrangement is about to take away.
        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        runtime.teardown(&spec.name).unwrap();

        // This session dies twice over, which is measured and not reasoned: the
        // request to stop reaches it first, and the death of the anchor would
        // take it anyway, since the anchor is pid 1 of the sandbox's pid
        // namespace and a pid 1 takes its namespace with it. So nothing here
        // discriminates, and no single change to how a teardown signals turns
        // this red. It stays because it is the only line that notices if BOTH
        // of those go away, and the second one goes away silently, by someone
        // dropping the pid namespace from the spec.
        // The session deliberately carries nothing naming the sandbox in its
        // environment, which is what anything spawned inside the box looks like
        // once a shell in there has cleared the variable.
        assert!(stopped_within_deadline(session.pid), "the session outlived the teardown");
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_completes_for_a_dead_anchor_whose_cgroup_the_runtime_cannot_read() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        // The session is the arrangement and not decoration: measured fifteen
        // times over, killing the anchor of a sandbox that has one leaves the
        // runtime unable to read the cgroup it stops the container through,
        // while killing the anchor of a sandbox that has none never does.
        runtime.join_session(&session_spec(&spec.name)).unwrap();
        unsafe { libc::kill(token.pid.0 as libc::pid_t, libc::SIGKILL) };
        assert!(stopped_within_deadline(token.pid.0), "the anchor outlived the kill");

        let torn_down = runtime.teardown(&spec.name);

        // A sandbox somebody killed is one hort has to be able to collect, and
        // the runtime losing the cgroup it would have stopped the container
        // through says nothing about whether anything is still standing. Failing
        // here leaves `down` reporting an error over a box that is already gone,
        // and the worktree it was told to remove on disk with no route to it.
        assert_eq!(torn_down, Ok(()));
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_stops_a_live_anchor_whose_cgroup_the_runtime_cannot_read() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let _cgroup_lost = CgroupPathAside::pointed_away_from(&sandbox, &spec.name);

        runtime.teardown(&spec.name).unwrap();

        // The same unreadable cgroup that reaches the runtime over a sandbox
        // already gone reaches it over one still running, and the two are told
        // apart by asking the kernel rather than by reading the failure. Taking
        // the failure for a finished teardown would report a box down while its
        // anchor still holds the worktree mounted, and whoever asked for the
        // teardown deletes that worktree next.
        assert!(stopped_within_deadline(token.pid.0), "the anchor outlived the teardown");
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_stops_a_sandbox_process_that_ignores_the_request_to_stop() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let session = runtime.join_session(&stop_ignoring_session(&spec.name)).unwrap();
        let ready = spec.workdir.join(READY_FOR_A_STOP);
        assert!(
            appeared_within_deadline(&ready),
            "the session never took charge of a stop request"
        );
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        runtime.teardown(&spec.name).unwrap();

        // Asking a process to stop is a request, and this sandbox holds one that
        // refuses it, which is the shape of the agent the request exists for in
        // the first place. A teardown that asks, waits and gives up leaves the
        // box standing behind a report that it is down, which is the same lie
        // this arm was opened to end, arriving through the remedy for it.
        assert!(stopped_within_deadline(session.pid), "the session survived the teardown");
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_sandbox_process_is_asked_to_stop_before_it_is_killed() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        runtime.join_session(&stop_reporting_session(&spec.name)).unwrap();
        let ready = spec.workdir.join(READY_FOR_A_STOP);
        assert!(
            appeared_within_deadline(&ready),
            "the session never took charge of a stop request"
        );
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        runtime.teardown(&spec.name).unwrap();

        // The anchor is a `sleep` and goes on the first signal either way, but a
        // session can be an agent halfway through writing a file into the
        // worktree, and the difference between a whole file and a truncated one
        // is the whole reason the order of a shutdown is fixed at all. Killing
        // outright saves nothing and spends exactly what the shutdown order is
        // there to protect.
        assert!(spec.workdir.join(ASKED_TO_STOP).exists(), "the session was killed unasked");
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn teardown_leaves_the_live_anchor_of_another_sandbox_running() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let stranger = SandboxName::new(&format!("{}-stranger", spec.name.as_str())).unwrap();
        // The refusal below is only worth reading if the enumeration a teardown
        // consults does name this anchor: a sandbox nothing can see is one
        // nothing could have stopped by mistake.
        let live = runtime.list_live().unwrap();
        assert!(live.iter().any(|entry| entry.id == spec.name), "the anchor is not enumerated");

        runtime.teardown(&stranger).unwrap();

        // The runtime knows nothing of this name, so the anchors it can reach
        // are the ones it was never asked about. Signalling by liveness instead
        // of by name would take down the box a person is working in on the
        // strength of a teardown aimed somewhere else entirely.
        assert!(
            Path::new(&format!("/proc/{}/ns/mnt", token.pid.0)).exists(),
            "the teardown of another name stopped this anchor"
        );
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn anchor_writes_its_streams_to_the_sandbox_log() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

        let token = runtime.start_anchor(&spec).unwrap();

        // The anchor outlives the command that started it, so an anchor left
        // holding the streams hort was invoked with holds them for the life of
        // the sandbox: a piped or redirected invocation never reaches EOF and its
        // reader waits on a `sleep infinity` that will never write.
        wait_for_anchor(token.pid.0);
        let log = sandbox_log_path(&sandbox.sandbox_dir());
        assert_eq!(fs::read_link(format!("/proc/{}/fd/1", token.pid.0)).unwrap(), log);
        assert_eq!(fs::read_link(format!("/proc/{}/fd/2", token.pid.0)).unwrap(), log);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_state_root_holds_no_sandbox_log() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

        let token = runtime.start_anchor(&spec).unwrap();

        // The log is written by processes the distribution labels, and the state
        // root carries a label those processes may not write to, so the one file
        // hort diagnoses a failed sandbox from is also the one the policy silences.
        // Writing it in both places would keep that failure and hand the reader two
        // halves of one account.
        wait_for_anchor(token.pid.0);
        assert!(!sandbox_log_path(&sandbox.state_dir()).exists());
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn the_anchor_reads_from_nothing_rather_than_from_what_invoked_hort() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let invocation_input = sandbox.state_root().join("invocation-input");
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);

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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
    fn a_running_sandbox_reports_the_session_joined_to_it() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let session = runtime.join_session(&session_spec(&spec.name)).unwrap();

        let reported = runtime.session_pids(&spec.name).unwrap();

        // Only a real box can say this: the anchor and the session land in the
        // sandbox's cgroup through the runtime's own bookkeeping, and where that
        // cgroup is on this host is decided by the systemd driver a rootless
        // container is forced onto. Equality rather than membership, because the
        // anchor is in that same cgroup and everything downstream counts this
        // list without reading it: one extra entry is a box that reports a
        // session nobody opened.
        assert_eq!(reported, vec![session.pid]);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn session_pids_counts_the_session_of_a_sandbox_whose_container_state_vanished() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        // Joined while the state is still there, because joining reads the very
        // files the arrangement is about to take away. It names the sandbox the
        // way every session `attach` opens does, so two processes declare this
        // sandbox by the time the count is asked, and an anchor found by that
        // declaration alone is the session as readily as the anchor, which then
        // counts the anchor in the session's place.
        let session = runtime.join_session(&declaring_session(&spec)).unwrap();
        wait_for_session_to_declare(session.pid, &spec.name);
        let _state_gone = ContainerStateAside::taken_from(&sandbox, &spec.name);

        let reported = runtime.session_pids(&spec.name).unwrap();

        // Those files are bookkeeping the anchor does not depend on, so the
        // session is as open after their loss as before it. Answered as unknown
        // instead, the count takes the box's idle down with it, and `ls` shows a
        // box a person is typing in with no session and no idle to its name.
        assert_eq!(reported, vec![session.pid]);
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces and a prepared rootfs (HORT_TEST_ROOTFS)"]
    #[serial]
    fn a_session_finds_a_writable_home_at_the_dedicated_path() {
        let Some(rootfs) = prepared_rootfs() else { return };
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec =
            OciSpec { mounts: vec![dotfile_mount(sandbox.state_root())], ..sandbox.spec(rootfs) };
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec =
            OciSpec { mounts: vec![dotfile_mount(sandbox.state_root())], ..sandbox.spec(rootfs) };
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::under_a_deep_runtime_root();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = sandbox.network();
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
        let sandbox_dir = sandbox.sandbox_dir();
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = sandbox.network();
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
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let spec = sandbox.spec(rootfs);
        let token = runtime.start_anchor(&spec).unwrap();
        wait_for_anchor(token.pid.0);
        let provider = sandbox.network();
        let port = a_declared_port();
        let listening = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let declaring_the_port = NetworkSpec {
            db_forwards: vec![DbForward { host: "127.0.0.1".to_string(), port }],
            ..open_network(&spec.name, token.pid.0)
        };
        provider.provision(&declaring_the_port).unwrap();

        runtime.join_session(&dialling_session(&spec.name, port)).unwrap();

        // Open egress is unfiltered by contract, and an open sandbox records no
        // ports at all. Reading that silence as an empty set would lock every
        // open sandbox out of the network it is entitled to, which is the shape
        // a fail-closed default takes when it is applied where nothing failed.
        // The port is declared because the sandbox's loopback reaches only what
        // the project declared, in this posture as in the other; what is under
        // test is whether the session is let through to it.
        assert!(reached_within_deadline(&listening));
        provider.teardown(&spec.name).unwrap();
        runtime.teardown(&spec.name).unwrap();
    }

    #[test]
    #[ignore = "needs unprivileged user namespaces"]
    #[serial]
    fn start_anchor_fails_with_container_runtime_failed_for_a_missing_rootfs() {
        let sandbox = ScratchSandbox::new();
        let runtime = sandbox.runtime();
        let absent = sandbox.state_root().join("no-such-rootfs");
        let spec = sandbox.spec(absent);

        let result = runtime.start_anchor(&spec);

        assert!(matches!(result, Err(HortError::ContainerRuntimeFailed { .. })));
    }
}
