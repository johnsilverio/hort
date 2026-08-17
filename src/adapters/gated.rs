//! The scratch sandbox the gated suites raise a real container on.
//!
//! Compiled only by the tests behind the privileged feature, and it exists
//! because those tests leave real things on the machine: a systemd scope, an
//! anchor, host-side helpers, and a directory the kernel has taken every
//! permission off. Each of those outlives the test that made it, so a test that
//! dies on an assertion leaves them standing unless something else takes them
//! away.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tempfile::TempDir;

use crate::adapters::pasta::PastaNetworkProvider;
use crate::adapters::runtime::LibcontainerRuntime;
use crate::domain::model::SandboxName;
use crate::ports::{ContainerRuntime, NetworkProvider, OciSpec};

/// A sandbox name nothing else on this machine answers to.
///
/// A sandbox name is the container id, and the container id names the systemd
/// scope the cgroup lands in, a namespace shared by everything this user runs.
/// Two sandboxes under one name therefore share one scope: the second joins the
/// scope the first made, and the first teardown stops the whole of it. A fixed
/// name here would reach a sandbox a person is working in and kill its anchor.
fn a_name_of_its_own() -> SandboxName {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
    SandboxName::new(&format!("gated-{}-{ordinal}", std::process::id())).unwrap()
}

/// The two throwaway roots a gated test runs a real sandbox under, the name that
/// sandbox is built with, and the teardown that runs whether the test reached
/// its own or died on an assertion.
pub(crate) struct ScratchSandbox {
    name: SandboxName,
    runtime_root: PathBuf,
    /// The throwaway directory the runtime root sits in, held so that dropping
    /// this takes the runtime root away with it however deep it was put.
    _scratch: TempDir,
    state_root: TempDir,
}

impl ScratchSandbox {
    pub(crate) fn new() -> Self {
        Self::rooted_at(Path::to_path_buf)
    }

    /// The same sandbox under a runtime root as deep as one gets. The variable
    /// naming that root is the user's to set, and a session bus or a container
    /// tool pointing it at a path of its own is enough; every other scratch path
    /// here lives directly under the system temporary directory and is therefore
    /// short, so nothing else can tell an address that fits from one that does
    /// not.
    pub(crate) fn under_a_deep_runtime_root() -> Self {
        Self::rooted_at(|scratch| {
            let deep = scratch
                .join("runtime-dir-on-a-long-organisation-path")
                .join("with-a-nested-hort-root");
            fs::create_dir_all(&deep).unwrap();
            deep
        })
    }

    fn rooted_at(runtime_root: impl FnOnce(&Path) -> PathBuf) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        Self {
            name: a_name_of_its_own(),
            runtime_root: runtime_root(scratch.path()),
            _scratch: scratch,
            state_root: tempfile::tempdir().unwrap(),
        }
    }

    pub(crate) fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub(crate) fn state_root(&self) -> &Path {
        self.state_root.path()
    }

    /// Where this sandbox's helpers keep their runtime files: the log, the pid
    /// files, and the record of the ports a session may reach.
    pub(crate) fn sandbox_dir(&self) -> PathBuf {
        self.runtime_root().join("sandboxes").join(self.name.as_str())
    }

    /// Where this sandbox's memory of itself lives, the half of it a restart does
    /// not make meaningless.
    pub(crate) fn state_dir(&self) -> PathBuf {
        self.state_root().join("sandboxes").join(self.name.as_str())
    }

    pub(crate) fn runtime(&self) -> LibcontainerRuntime {
        LibcontainerRuntime::new(self.runtime_root().to_path_buf())
    }

    pub(crate) fn network(&self) -> PastaNetworkProvider {
        PastaNetworkProvider::new(self.runtime_root().to_path_buf())
    }

    /// The sandbox to build from `rootfs`, with the worktree it binds already on
    /// disk.
    pub(crate) fn spec(&self, rootfs: PathBuf) -> OciSpec {
        let state_dir = self.state_dir();
        let workdir = state_dir.join(format!("worktree-{}", self.name.as_str()));
        fs::create_dir_all(&workdir).unwrap();
        OciSpec {
            name: self.name.clone(),
            rootfs,
            overlay: state_dir.join("overlay"),
            workdir,
            env: vec![("HORT_SANDBOX".to_string(), self.name.as_str().to_string())],
            mounts: Vec::new(),
            drop_ins: Vec::new(),
            resources: None,
        }
    }
}

impl Drop for ScratchSandbox {
    fn drop(&mut self) {
        // In the order a sandbox has to come down in, and every step attempted
        // whatever the one before it did, because a helper that will not die is
        // exactly the case where stopping early leaves the container standing.
        // Nothing here may fail loudly: a panic raised while a test is already
        // unwinding aborts the process and takes the failed assertion with it.
        let _ = self.network().teardown(&self.name);
        let _ = self.runtime().teardown(&self.name);
        take_back_traversal(self.state_root());
    }
}

/// Give the owner back the right to enter and read every directory under this
/// one, so what is inside can be removed.
///
/// Mounting a sandbox's writable layer leaves a work directory behind that the
/// kernel takes every permission off, its owner included. A recursive remove
/// stops at one of those, and the one a scratch directory is taken away by
/// reports nothing when it does, so the directory simply stays on the machine.
fn take_back_traversal(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            take_back_traversal(&entry.path());
        }
    }
}
