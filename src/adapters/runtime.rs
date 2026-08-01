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
//! The spec the container is built from is assembled by a pure function over
//! plain data, which is what keeps the interesting decisions (empty capability
//! sets, the id mapping, the namespace set, the resource ceiling) testable
//! without a kernel.

use std::ffi::CString;
use std::fs;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
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

use crate::domain::error::HortError;
use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxName};
use crate::ports::{
    ContainerRegistry, ContainerRuntime, OciSpec, RegistryEntry, ResourceLimits, SessionProbe,
};

const SANDBOXES_DIR: &str = "sandboxes";
const BUNDLE_DIR: &str = "bundle";
const CONFIG_FILE: &str = "config.json";
const UPPER_LAYER: &str = "upper";
const WORK_LAYER: &str = "work";
const MERGED_ROOT: &str = "merged";
const WORKDIR: &str = "/workdir";
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CPU_PERIOD_USEC: u64 = 100_000;
/// The byte both handshakes send; only its arrival, never its value, carries
/// meaning, and the closed pipe that yields none is the abort signal.
const HANDSHAKE: [u8; 1] = [1];
const ANCHOR_FAILED: u8 = 0;
const ANCHOR_STARTED: u8 = 1;

/// A `ContainerRuntime` (and the read ports the embedded runtime will also serve)
/// for builds without the in-process container runtime.
pub struct NullRuntime;

impl ContainerRuntime for NullRuntime {
    fn start_anchor(&self, _spec: &OciSpec) -> Result<LivenessToken, HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn join_session(&self, _name: &SandboxName) -> Result<(), HortError> {
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

    fn bundle_dir(&self, name: &SandboxName) -> PathBuf {
        self.state_root.join(SANDBOXES_DIR).join(name.as_str()).join(BUNDLE_DIR)
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
        start_container(&spec.name, &bundle, &self.youki_root)
    }
}

impl ContainerRuntime for LibcontainerRuntime {
    fn start_anchor(&self, spec: &OciSpec) -> Result<LivenessToken, HortError> {
        let owner = host_owner(&spec.workdir)?;
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
                let outcome = self.build_sandbox(spec, ready_writer, released_reader);
                report_and_exit(report_writer, outcome);
            }
            child => {
                drop(ready_writer);
                drop(released_reader);
                drop(report_writer);
                let mapping = install_id_mapping(child, owner, ready_reader, released_writer);
                let report = read_report(report_reader);
                reap(child);
                mapping?;
                liveness_token(report?)
            }
        }
    }

    fn join_session(&self, _name: &SandboxName) -> Result<(), HortError> {
        // Sessions arrive with attach, which is where what a session runs (its
        // shell, its environment, its working directory) is decided; there is
        // nothing to join a process into before that.
        Err(HortError::RuntimeUnavailable)
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
    io::pipe().map_err(|err| runtime_failure(format!("start_anchor: creating a pipe: {err}")))
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
fn start_container(name: &SandboxName, bundle: &Path, youki_root: &Path) -> Result<u32, String> {
    fs::create_dir_all(youki_root)
        .map_err(|err| format!("creating {}: {err}", youki_root.display()))?;

    let mut container = ContainerBuilder::new(name.as_str().to_string(), SyscallType::default())
        .with_root_path(youki_root)
        .map_err(|err| format!("rooting the container state at {}: {err}", youki_root.display()))?
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

/// Hand the outcome to the parent and leave at once, without unwinding and
/// without at-exit handlers: everything they would clean up is still owned by
/// the process on the other side of the fork.
fn report_and_exit(mut report: PipeWriter, outcome: Result<u32, String>) -> ! {
    let (message, code) = match outcome {
        Ok(pid) => {
            let mut message = vec![ANCHOR_STARTED];
            message.extend_from_slice(&pid.to_le_bytes());
            (message, 0)
        }
        Err(detail) => {
            let mut message = vec![ANCHOR_FAILED];
            message.extend_from_slice(detail.as_bytes());
            (message, 1)
        }
    };
    let _ = report.write_all(&message);
    drop(report);
    unsafe { libc::_exit(code) }
}

fn read_report(mut report: PipeReader) -> Result<u32, HortError> {
    let mut message = Vec::new();
    report.read_to_end(&mut message).map_err(|err| {
        runtime_failure(format!("start_anchor: reading the sandbox report: {err}"))
    })?;

    if let [ANCHOR_STARTED, pid @ ..] = message.as_slice()
        && let Ok(pid) = <[u8; 4]>::try_from(pid)
    {
        return Ok(u32::from_le_bytes(pid));
    }
    if let [ANCHOR_FAILED, detail @ ..] = message.as_slice() {
        return Err(runtime_failure(format!("start_anchor: {}", String::from_utf8_lossy(detail))));
    }
    Err(runtime_failure("start_anchor: the sandbox process exited without reporting an anchor"))
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
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use serial_test::serial;
    use tempfile::TempDir;

    const ANCHOR_DEADLINE: Duration = Duration::from_secs(5);

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
