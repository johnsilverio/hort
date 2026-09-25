//! End-to-end CLI tests: drive the built `hort` binary and assert on its exit
//! code, stdout, and stderr. Each test points the binary at a throwaway state
//! root (via `XDG_STATE_HOME`) and a throwaway git repository, so the real user
//! state is never touched.
//!
//! The gated ones go further and raise a real sandbox on the machine running
//! them, so each of those drives the binary through a `ScratchSandbox`, which
//! owns the name it is built under and takes it down again whatever the test
//! did.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command as GitCommand, Stdio};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use hort::adapters::liveness::ProcLivenessProbe;
use hort::adapters::metadata::FileMetadataStore;
use hort::adapters::notify::FileNotifyProvider;
use hort::adapters::pasta::PastaNetworkProvider;
use hort::adapters::runtime::LibcontainerRuntime;
use hort::domain::model::{LivenessToken, SandboxName};
use hort::domain::mounts::GIT_OBJECTS;
use hort::ports::{
    ContainerRuntime, LivenessProbe, MetadataStore, NetworkProvider, NotifyProvider,
};
use predicates::prelude::*;
use tempfile::TempDir;

/// How long a run of `hort` is given to end on its own before the test that
/// drives it stops waiting. Long enough that no slow machine reaches it by
/// working, short enough that a build which stopped to ask says so in seconds.
const ANSWERED_BY: Duration = Duration::from_secs(10);

fn git(dir: &Path, args: &[&str]) {
    let status = GitCommand::new("git").current_dir(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// A throwaway git repository with one commit on `main`, returned with its
/// canonicalized path. The `TempDir` guard must outlive the test.
fn temp_git_repo() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    git(&path, &["init", "-b", "main"]);
    fs::write(path.join("README.md"), "seed\n").unwrap();
    git(&path, &["add", "README.md"]);
    git(
        &path,
        &[
            "-c",
            "user.name=hort tests",
            "-c",
            "user.email=tests@hort.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    (dir, path)
}

/// Write an orphaned sandbox's record under `state_root`: a real on-disk
/// `metadata.json` with a null liveness token, matching the camelCase schema the
/// metadata store reads.
fn write_orphaned_record(state_root: &Path, name: &str) {
    let sandbox_dir = state_root.join("sandboxes").join(name);
    fs::create_dir_all(&sandbox_dir).unwrap();
    let worktree = sandbox_dir.join(format!("worktree-{name}")).display().to_string();
    let overlay = sandbox_dir.join("overlay").display().to_string();
    let metadata = format!(
        r#"{{
  "schemaVersion": 1,
  "name": "{name}",
  "branch": "{name}",
  "worktreePath": "{worktree}",
  "overlayPath": "{overlay}",
  "createdAt": "2026-06-11T12:00:00Z",
  "lastAttachAt": "2026-06-11T12:00:00Z",
  "notifyChannel": null,
  "watcherPid": null,
  "token": null
}}"#
    );
    fs::write(sandbox_dir.join("metadata.json"), metadata).unwrap();
}

/// Write a record whose anchor the kernel no longer has: a real on-disk
/// `metadata.json` carrying a liveness token for a process that has already
/// exited. The mount-namespace inode is one no namespace can have, so a reused
/// pid cannot make this record read as live either.
fn write_record_with_a_dead_anchor(state_root: &Path, name: &str) {
    let sandbox_dir = state_root.join("sandboxes").join(name);
    fs::create_dir_all(&sandbox_dir).unwrap();
    let worktree = sandbox_dir.join(format!("worktree-{name}")).display().to_string();
    let overlay = sandbox_dir.join("overlay").display().to_string();
    let reaped = GitCommand::new("true").spawn().unwrap();
    let dead = reaped.id();
    reaped.wait_with_output().unwrap();
    let metadata = format!(
        r#"{{
  "schemaVersion": 1,
  "name": "{name}",
  "branch": "{name}",
  "worktreePath": "{worktree}",
  "overlayPath": "{overlay}",
  "createdAt": "2026-06-11T12:00:00Z",
  "lastAttachAt": "2026-06-11T12:00:00Z",
  "notifyChannel": null,
  "watcherPid": null,
  "token": {{ "pid": {dead}, "mntNsInode": 1 }}
}}"#
    );
    fs::write(sandbox_dir.join("metadata.json"), metadata).unwrap();
}

/// A throwaway XDG config root holding one global hort config, returned with its
/// canonicalized path. Every test that resolves configuration points the binary
/// at one of these, so the configuration on the developer's own machine can never
/// reach a test run.
fn temp_config_home(global: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    fs::create_dir_all(path.join("hort")).unwrap();
    fs::write(path.join("hort").join("config.json"), global).unwrap();
    (dir, path)
}

/// A throwaway XDG config root with nothing in it, returned with its
/// canonicalized path: what the host of a first run looks like, before hort has
/// ever written anything for this user.
fn temp_config_home_of_a_first_run() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

/// A throwaway home directory, returned with its canonicalized path. Under the
/// user's real home rather than under `/tmp`, because a path hort derives from a
/// home is only as faithful as the home it was handed, and two defects have
/// already shipped past a green suite from a scratch directory that did not
/// carry a property of the path it stood in for. It holds only what the test
/// puts in it, so the guard takes back everything it made.
fn temp_host_home() -> (TempDir, PathBuf) {
    let real_home = std::env::home_dir().expect("the test runner has a home directory");
    let dir = TempDir::new_in(real_home).unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

/// A throwaway directory to hand hort as the whole of its `PATH`, holding one
/// executable file per name in `programs`, returned with its canonicalized path.
///
/// Nothing here is ever run: what hort reads off the `PATH` is that a file of
/// that name is there and carries an execute bit, so an empty file is a faithful
/// stand-in and no test has to install anything to say a host has it.
fn temp_path_holding(programs: &[&str]) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    for program in programs {
        let binary = path.join(program);
        fs::write(&binary, "").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, path)
}

/// The prepared rootfs the end-to-end test builds a sandbox from, or `None`
/// after saying what is missing, so a host without one reports why it skipped
/// instead of failing.
fn prepared_rootfs() -> Option<String> {
    let Ok(configured) = std::env::var("HORT_TEST_ROOTFS") else {
        eprintln!("skipped: set HORT_TEST_ROOTFS to a prepared rootfs directory to run this");
        return None;
    };
    if !Path::new(&configured).is_dir() {
        eprintln!("skipped: rootfs directory '{configured}' does not exist, prepare it first");
        return None;
    }
    Some(configured)
}

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
    SandboxName::new(&format!("gated-cli-{}-{ordinal}", std::process::id())).unwrap()
}

/// The two throwaway XDG roots a gated test drives the binary under, the name it
/// drives it with, and the teardown that runs whether the test reached its own
/// `down` or died before it.
///
/// Declare it after the other fixtures, so that it is the first of them to go:
/// a sandbox comes down before the directory it binds, and the folder a no-git
/// run mounts belongs to a fixture of its own.
struct ScratchSandbox {
    name: SandboxName,
    state_home: TempDir,
    runtime_dir: TempDir,
}

impl ScratchSandbox {
    fn new() -> Self {
        Self {
            name: a_name_of_its_own(),
            state_home: TempDir::new().unwrap(),
            runtime_dir: TempDir::new().unwrap(),
        }
    }

    fn name(&self) -> &SandboxName {
        &self.name
    }

    fn state_home(&self) -> &Path {
        self.state_home.path()
    }

    fn runtime_dir(&self) -> &Path {
        self.runtime_dir.path()
    }

    /// Where hort keeps what remembers the sandbox, which is a component deeper
    /// than the variable that decides it. A teardown handed the variable's own
    /// value works on an empty directory, finds nothing to stop and says so
    /// nowhere.
    fn state_root(&self) -> PathBuf {
        self.state_home().join("hort")
    }

    /// Where hort keeps what a restart makes meaningless, one component under
    /// the variable for the same reason as above.
    fn runtime_root(&self) -> PathBuf {
        self.runtime_dir().join("hort")
    }

    /// This sandbox's memory of itself: the record, the overlay, the worktree.
    fn state_dir(&self) -> PathBuf {
        self.state_root().join("sandboxes").join(self.name.as_str())
    }

    /// Where this sandbox's host-side helpers keep their runtime files: the log,
    /// the pid files, and the record of the ports a session may reach.
    fn sandbox_dir(&self) -> PathBuf {
        self.runtime_root().join("sandboxes").join(self.name.as_str())
    }
}

impl Drop for ScratchSandbox {
    fn drop(&mut self) {
        // In the order a sandbox has to come down in, and every step attempted
        // whatever the one before it did, because a helper that will not die is
        // exactly the case where stopping early leaves the container standing.
        // Nothing here may fail loudly: a panic raised while a test is already
        // unwinding aborts the process and takes the failed assertion with it.
        let runtime_root = self.runtime_root();
        let _ =
            FileNotifyProvider::new(self.state_root(), runtime_root.clone()).teardown(&self.name);
        let _ = PastaNetworkProvider::new(runtime_root.clone()).teardown(&self.name);
        let _ = LibcontainerRuntime::new(runtime_root).teardown(&self.name);
        take_back_traversal(self.state_home());
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

#[test]
fn cli_ls_exits_zero_with_no_sandboxes() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success();
}

#[test]
fn cli_ls_creates_no_state_root_on_a_host_with_no_sandbox() {
    let xdg = TempDir::new().unwrap();
    // The variable names a directory that is not there either, so the run has
    // no existing state root to canonicalize and no existing parent to make one
    // in: the arrangement that tells creating the root from creating nothing.
    let state_home = xdg.path().canonicalize().unwrap().join("state");
    let (_repo, repo_path) = temp_git_repo();

    let ran = Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &state_home)
        .current_dir(&repo_path)
        .timeout(ANSWERED_BY)
        .arg("ls")
        .output()
        .unwrap();

    // The listing has to have answered for its silence on disk to mean
    // anything: a run that failed before reaching the disk creates nothing too.
    assert_eq!(ran.status.code(), Some(0), "{}", String::from_utf8_lossy(&ran.stderr));
    assert!(
        !state_home.exists(),
        "listing a host with no sandbox created {}",
        state_home.display()
    );
}

#[test]
fn cli_ls_reports_orphaned_sandbox() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    write_orphaned_record(&state_root, "demo");
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("orphaned"));
}

#[test]
fn cli_down_unknown_name_prints_canonical_error_to_stderr() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .args(["down", "ghost"])
        .assert()
        .code(1)
        .stderr("no sandbox named 'ghost' (run 'hort ls' to see what exists)\n");
}

#[test]
fn cli_attach_unknown_name_prints_canonical_error_to_stderr() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .args(["attach", "ghost"])
        .assert()
        .code(1)
        .stderr("no sandbox named 'ghost' (run 'hort ls' to see what's alive)\n");
}

#[test]
fn cli_attach_reports_a_sandbox_whose_anchor_is_gone() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    write_record_with_a_dead_anchor(&state_root, "demo");
    let (_repo, repo_path) = temp_git_repo();

    // A record hort still has and an anchor the kernel no longer does are a
    // different answer from a name nothing knows, and the repair they point at
    // is different too. Only the kernel can tell them apart, so this is also
    // what proves the answer was asked of it rather than read off the record.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .args(["attach", "demo"])
        .assert()
        .code(1)
        .stderr(
            "sandbox 'demo' is not running (run 'hort up demo' to start it, or 'hort prune' to clean up the stale record)\n",
        );
}

#[test]
fn cli_down_removes_orphaned_sandbox() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    let runtime = TempDir::new().unwrap();
    let runtime_root = runtime.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();

    write_orphaned_record(&state_root, "demo");
    let worktree_path = state_root.join("sandboxes").join("demo").join("worktree-demo");
    git(&repo_path, &["worktree", "add", "-b", "demo", worktree_path.to_str().unwrap()]);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["down", "demo"])
        .assert()
        .success();

    assert!(!worktree_path.exists());
    assert!(!state_root.join("sandboxes").join("demo").exists());
}

#[test]
fn cli_up_reports_a_malformed_global_config() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(r#"{ "rootfs": "#);
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("config.json"));
}

#[test]
fn cli_up_reports_a_malformed_project_config() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let (_repo, repo_path) = temp_git_repo();
    fs::write(repo_path.join(".hort.json"), r#"{ "egress": "#).unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(".hort.json"));
}

#[test]
fn cli_up_reads_configuration_from_the_project_root_when_run_from_a_subdirectory() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let (_repo, repo_path) = temp_git_repo();
    let project_config = repo_path.join(".hort.json");
    fs::write(&project_config, r#"{ "egress": "#).unwrap();
    let nested = repo_path.join("crates").join("api");
    fs::create_dir_all(&nested).unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&nested)
        .args(["up", "demo"])
        .assert()
        .code(1)
        // The whole path, not just the file name: the message hort prints when
        // no rootfs is configured names `.hort.json` as a place to declare one,
        // so a shorter assertion would pass on a build that never read this file.
        .stderr(predicate::str::contains(project_config.display().to_string()));
}

#[test]
fn cli_up_names_the_rootfs_it_could_not_find() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(r#"{ "rootfs": "/nonexistent/hort/rootfs" }"#);
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"])
        .assert()
        .code(1)
        .stderr(
            "rootfs directory '/nonexistent/hort/rootfs' does not exist — prepare it first with podman export, debootstrap or umoci unpack\n",
        );
}

#[test]
fn cli_up_names_the_git_it_could_not_find() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let home = TempDir::new().unwrap();
    // A marked folder holding no repository, which is the arm that exists for a
    // project without git and is the one a host without the binary defeats.
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();
    fs::write(project_path.join(".hort.json"), "{}").unwrap();
    // A rootfs that answers yes to everything the configuration chain asks it,
    // so the run reaches the host question this test is about instead of
    // stopping one step earlier on something it configured badly.
    let rootfs = TempDir::new().unwrap();
    let rootfs_path = rootfs.path().canonicalize().unwrap();
    let shell = rootfs_path.join("bin").join("sh");
    fs::create_dir_all(shell.parent().unwrap()).unwrap();
    fs::write(&shell, "").unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let workdir = rootfs_path.join("workdir");
    fs::create_dir(&workdir).unwrap();
    fs::set_permissions(&workdir, fs::Permissions::from_mode(0o1777)).unwrap();
    let global = format!(r#"{{ "rootfs": "{}" }}"#, rootfs_path.display());
    let (_config, config_home) = temp_config_home(&global);
    let (_lookup, path_without_git) = temp_path_holding(&["pasta"]);

    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("PATH", &path_without_git)
        .current_dir(&project_path)
        .timeout(ANSWERED_BY)
        .args(["up", "demo", "-d"])
        .assert()
        .code(1)
        // The whole message and never a fragment of it: what this host produces
        // without the check is git's own complaint about rev-parse, carried up
        // from inside the adapter, and a user reading that is told neither what
        // is missing nor what to do about it.
        .stderr("git not found on PATH — hort needs it to prepare the sandbox worktree\n");
}

#[test]
fn cli_up_falls_back_to_defaults_without_a_terminal() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        // A finite wait, so a build that stops to ask something comes back red
        // here instead of never coming back at all.
        .timeout(ANSWERED_BY)
        .args(["up", "demo"])
        .assert()
        .code(1)
        .stderr(
            "no rootfs configured — set \"rootfs\" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json\n",
        );

    assert!(
        !config_home.join("hort").join("config.json").exists(),
        "and nothing was written on the way, because nobody was there to be asked"
    );
}

#[test]
fn cli_up_opens_onboarding_on_a_first_run() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    // A home of its own, because the dialogue offers what the home it is given
    // holds: pointed at the real one, how many questions get asked would be
    // decided by the machine the suite runs on.
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"]);

    // Two answers, both no, which is what a home holding none of the offered
    // dotfiles and no agent credentials leaves to ask: whether there is a
    // prepared rootfs, and whether to raise a desktop notification.
    let transcript = typed_at_the_terminal(hort, "nn");

    assert!(
        config_home.join("hort").join("config.json").exists(),
        "a first run at a terminal writes the configuration it just asked about: {transcript}"
    );
    assert!(
        transcript.contains("no rootfs configured"),
        "and then goes on with the command that was typed, answering out of what it wrote: {transcript}"
    );
}

#[test]
fn cli_config_refuses_without_a_terminal() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    let home = TempDir::new().unwrap();
    let anywhere = TempDir::new().unwrap();

    // The string and the code, never `.failure()`: a subcommand the binary does
    // not have is refused too, and today this very command line comes back with
    // clap's `unrecognized subcommand` and a 2.
    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("config")
        .assert()
        .code(1)
        .stderr(
            "hort config needs a terminal to ask you what to configure; no flag replaces it (run it from an interactive shell, or write ~/.config/hort/config.json by hand)\n",
        );
}

#[test]
fn cli_config_asks_nothing_when_stdin_is_redirected_at_a_terminal() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("config");

    let transcript = output_only_terminal(hort);

    // The whole of what it had to say, so a question asked before the refusal
    // fails here. Which of the two facts hort reads is invisible everywhere
    // else: with stderr piped as well, the prompts refuse on their own and
    // answer with this very string.
    assert_eq!(
        transcript,
        "hort config needs a terminal to ask you what to configure; no flag replaces it (run it from an interactive shell, or write ~/.config/hort/config.json by hand)\r\n"
    );
}

#[test]
fn cli_config_prints_the_advisory_its_dialogue_produced() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    // A home of its own, because the dialogue offers what the home it is given
    // holds: pointed at the real one, how many questions get asked would be
    // decided by the machine the suite runs on.
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("config");

    // Two answers, both no, which is what a home holding none of the offered
    // dotfiles and no agent credentials leaves to ask: whether there is a
    // prepared rootfs, and whether to raise a desktop notification.
    let transcript = typed_at_the_terminal(hort, "nn");

    // The prefix and not the sentence: what the dialogue had to say is the
    // generator's prose and may be reworded, while a run that prints none of it
    // has dropped a degradation hort promised to report.
    assert!(
        transcript.contains("warning: "),
        "what the dialogue found reaches the terminal as an advisory: {transcript}"
    );
}

#[test]
fn cli_config_asks_before_overwriting_an_existing_config() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(r#"{ "rootfs": "/from/an/earlier/run" }"#);
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let global = config_home.join("hort").join("config.json");
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("config");

    // Three noes for a flow that gets to ask one question, so a run that never
    // asked it answers the two the dialogue asks instead and is caught below,
    // rather than holding the terminal until the deadline.
    let transcript = typed_at_the_terminal(hort, "nnn");

    assert!(
        transcript.contains(&global.display().to_string()),
        "the file already there is what it asks about: {transcript}"
    );
    assert_eq!(
        fs::read_to_string(&global).unwrap(),
        r#"{ "rootfs": "/from/an/earlier/run" }"#,
        "and a no leaves it exactly as it was: {transcript}"
    );
}

#[test]
fn cli_config_under_force_overwrites_without_asking() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(r#"{ "rootfs": "/from/an/earlier/run" }"#);
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let global = config_home.join("hort").join("config.json");
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .args(["config", "--force"]);

    // Two noes, the whole dialogue on a home holding nothing it offers. A run
    // that still asked about the overwrite would spend the first of them saying
    // no to that and leave the file below untouched.
    let transcript = typed_at_the_terminal(hort, "nn");

    let written = fs::read_to_string(&global).unwrap();
    assert!(
        !written.contains("/from/an/earlier/run"),
        "what was there is gone, with nobody asked about it: {transcript}"
    );
}

#[test]
fn cli_up_proceeds_against_the_configuration_the_dialogue_wrote() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    // A home of its own, because the dialogue offers what the home it is given
    // holds: pointed at the real one, how many questions get asked would be
    // decided by the machine the suite runs on.
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"]);

    // A yes and a path, which is what makes the answer readable at all: with the
    // rootfs question refused, a configuration naming nothing and the one the
    // dialogue just wrote send `up` to the very same error, and a build reading
    // either of them looks right.
    let transcript = typed_at_the_terminal(hort, "y/nonexistent/hort/rootfs\nn");

    // What the run ends on, never what it contains: the dialogue says this same
    // sentence as an advisory while the person is still there to fix it, so only
    // the last word is `up` answering out of a configuration rather than the
    // dialogue answering about a path it was handed.
    assert!(
        transcript.trim_end().ends_with(
            "rootfs directory '/nonexistent/hort/rootfs' does not exist — prepare it first with podman export, debootstrap or umoci unpack"
        ),
        "the command that was typed goes on against the file the dialogue just wrote: {transcript}"
    );
}

#[test]
fn cli_config_completes_on_its_own_at_a_terminal() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    // A home of its own, because the dialogue offers what the home it is given
    // holds: pointed at the real one, how many questions get asked would be
    // decided by the machine the suite runs on.
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("config");

    // Two answers, both no, which is what a home holding none of the offered
    // dotfiles and no agent credentials leaves to ask: whether there is a
    // prepared rootfs, and whether to raise a desktop notification.
    let (how_it_ended, transcript) = ended_at_the_terminal(hort, "nn");

    // The status and not a line of the transcript: a run that asks one question
    // more than it has answers for is still holding the terminal when the
    // deadline reaches it, and comes back as a signal rather than as a status.
    assert_eq!(
        how_it_ended.code(),
        Some(0),
        "the dialogue is the whole of this command, so it runs out of questions and ends by itself: {transcript}"
    );
}

#[test]
fn cli_doctor_reports_a_host_with_pasta_differently_from_one_without() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let home = TempDir::new().unwrap();
    let anywhere = TempDir::new().unwrap();
    let (_with, path_with_pasta) = temp_path_holding(&["pasta"]);
    let (_without, path_without_pasta) = temp_path_holding(&[]);

    let doctor = |lookup: &Path| {
        Command::cargo_bin("hort")
            .unwrap()
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", &xdg_root)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("PATH", lookup)
            .current_dir(anywhere.path())
            .timeout(ANSWERED_BY)
            .arg("doctor")
            .output()
            .unwrap()
            .stdout
    };

    let where_pasta_is = doctor(&path_with_pasta);
    let where_it_is_not = doctor(&path_without_pasta);

    // Two hosts differing in one thing, because a report that prints a fixed
    // list of capability names looks right on any host and says the same thing
    // on both of these.
    assert_ne!(
        String::from_utf8_lossy(&where_pasta_is),
        String::from_utf8_lossy(&where_it_is_not),
        "the report is the same text on a host that has pasta and one that does not"
    );
}

#[test]
fn cli_doctor_gates_on_the_pasta_it_found() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let home = TempDir::new().unwrap();
    let anywhere = TempDir::new().unwrap();
    // git is on both, so the one thing the two hosts differ in is the one this
    // test is named after. It is on the open arm because the gate asks for it
    // too, and on the shut arm so that arm is shut by the absent pasta.
    let (_with, path_with_pasta) = temp_path_holding(&["pasta", "git"]);
    let (_without, path_without_pasta) = temp_path_holding(&["git"]);

    let doctor = |lookup: &Path| {
        Command::cargo_bin("hort")
            .unwrap()
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", &xdg_root)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("PATH", lookup)
            .current_dir(anywhere.path())
            .timeout(ANSWERED_BY)
            .arg("doctor")
            .output()
            .unwrap()
            .status
            .code()
    };

    // Both arms, because a gate that answers the same number on every host
    // passes either one alone. The zero arm reads this machine's kernel for the
    // user-namespace half, the way the rootfs message above reads it to get
    // past the same check.
    assert_eq!(doctor(&path_with_pasta), Some(0), "a host that can build a sandbox gates open");
    assert_ne!(doctor(&path_without_pasta), Some(0), "and one that cannot gates shut");
}

#[test]
fn cli_doctor_still_reports_when_the_gate_is_closed() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let home = TempDir::new().unwrap();
    let anywhere = TempDir::new().unwrap();
    let (_without, path_without_pasta) = temp_path_holding(&[]);

    let ran = Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("PATH", &path_without_pasta)
        .current_dir(anywhere.path())
        .timeout(ANSWERED_BY)
        .arg("doctor")
        .output()
        .unwrap();

    assert_ne!(ran.status.code(), Some(0), "the arrangement is a host that cannot build one");
    // The report is what somebody runs doctor for, and the host it matters most
    // on is the one that gates shut. Raised as an error instead, the whole thing
    // would go to stderr as a single line and this would be empty.
    assert!(!ran.stdout.is_empty(), "and it still says what it found");
}

#[test]
fn cli_doctor_reports_a_malformed_configuration_instead_of_refusing() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    // Only the project layer is broken, so what the report names is unambiguous.
    let (_config, config_home) = temp_config_home("{}");
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();
    fs::write(project_path.join(".hort.json"), r#"{ "rootfs": "#).unwrap();
    // A host the gate opens on, so the zero this test ends on can only be the
    // gate's answer and never a second reason to leave with one.
    let (_with, path_with_pasta) = temp_path_holding(&["pasta", "git"]);

    let ran = Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("PATH", &path_with_pasta)
        .current_dir(&project_path)
        .timeout(ANSWERED_BY)
        .arg("doctor")
        .output()
        .unwrap();

    // The whole path and never the bare file name, which was the first form of
    // this assertion and could not discriminate: the canonical no-rootfs message
    // names ".hort.json" too, so a build that quietly threw the unreadable file
    // away and reported an empty configuration matched it. Only the parser's own
    // complaint carries the path of the file it choked on.
    let broken = project_path.join(".hort.json");
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains(&broken.display().to_string()),
        "{}",
        String::from_utf8_lossy(&ran.stdout)
    );
    // The gate's answer about this machine, not the parser's about this project.
    // A read that rose through the command would leave here with the error's
    // code and nothing at all on stdout.
    assert_eq!(ran.status.code(), Some(0), "{}", String::from_utf8_lossy(&ran.stderr));
}

#[test]
fn cli_doctor_leaves_a_first_run_host_as_it_found_it() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home_of_a_first_run();
    let home = TempDir::new().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let anywhere = TempDir::new().unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("HOME", &home_path)
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .arg("doctor");

    // Nothing typed, at a terminal, on a host hort has never been set up on:
    // the arrangement that opens the first-run dialogue for every command that
    // needs a configuration.
    let (how_it_ended, transcript) = ended_at_the_terminal(hort, "");

    assert!(
        how_it_ended.code().is_some(),
        "a run that stopped to ask is still holding the terminal at the deadline and comes back as a signal: {transcript}"
    );
    assert!(
        !config_home.join("hort").join("config.json").exists(),
        "and a read-only report writes nothing, least of all a configuration nobody asked for: {transcript}"
    );
}

#[test]
fn cli_doctor_creates_no_state_root_on_a_first_run_host() {
    let xdg = TempDir::new().unwrap();
    // The variable names a directory that is not there either. It is the
    // stronger arrangement: a report that made the parent and stopped short of
    // the root would still have written to a host that promised it nothing.
    let state_home = xdg.path().canonicalize().unwrap().join("state");
    let (_config, config_home) = temp_config_home_of_a_first_run();
    let home = TempDir::new().unwrap();
    let anywhere = TempDir::new().unwrap();

    let ran = Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(anywhere.path())
        .timeout(ANSWERED_BY)
        .arg("doctor")
        .output()
        .unwrap();

    // The report first, because a run that failed before reaching the disk
    // creates nothing too and would pass the assertion below for free.
    assert!(
        !ran.stdout.is_empty(),
        "no report was printed: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        !state_home.exists(),
        "a read-only report created {} on a host that had no state root",
        state_home.display()
    );
}

#[test]
fn cli_up_leaves_no_sandbox_behind_when_it_cannot_build() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    let (_config, config_home) = temp_config_home(r#"{ "rootfs": "/nonexistent/hort/rootfs" }"#);
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .args(["up", "demo"])
        .assert()
        .code(1);

    // A build that stops on its preconditions has taken nothing yet, and what it
    // would have taken is expensive to hand back: a branch and a worktree the
    // user then has to recognize as debris, on a name a later run reads as
    // half-built.
    assert!(!state_root.join("sandboxes").join("demo").exists());
    let branches = GitCommand::new("git")
        .current_dir(&repo_path)
        .args(["branch", "--list", "demo"])
        .output()
        .unwrap();
    assert!(branches.stdout.is_empty());
}

#[test]
fn cli_up_refuses_a_directory_that_is_not_a_project() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    // A configuration with no rootfs, so the refusal has to come before the host
    // preconditions to be the message the user reads. Standing in the wrong
    // directory is what the person got wrong, and sending them off to prepare a
    // rootfs first would send them to fix something else.
    let (_config, config_home) = temp_config_home("{}");
    let plain = TempDir::new().unwrap();
    let plain_path = plain.path().canonicalize().unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&plain_path)
        .args(["up", "demo"])
        .assert()
        .code(1)
        .stderr(format!(
            "'{}' is not a project — run hort from a git repository, or add a .hort.json there to sandbox the directory itself\n",
            plain_path.display()
        ));
}

#[test]
fn cli_up_accepts_a_git_mode_flag() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    let plain = TempDir::new().unwrap();
    let plain_path = plain.path().canonicalize().unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&plain_path)
        .args(["up", "demo", "--git", "clone"])
        .assert()
        // A flag `up` does not carry is refused by the parser with exit 2, so a
        // run that reaches hort's own refusal of this directory is the proof the
        // flag is part of the command line. Where the value then goes is not
        // visible from outside without a prepared rootfs.
        .code(1);
}

#[test]
fn cli_ls_lists_sandboxes_despite_a_malformed_project_config() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    let (_config, config_home) = temp_config_home("{}");
    write_orphaned_record(&state_root, "demo");
    let (_repo, repo_path) = temp_git_repo();
    fs::write(repo_path.join(".hort.json"), r#"{ "rootfs": "#).unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"));
}

/// Report a libc call that failed as the errno that caused it, so a step of a
/// child's preparation which did not happen is told apart from a run that
/// happened and answered.
fn checked(returned: libc::c_int) -> std::io::Result<()> {
    match returned {
        -1 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}

#[test]
fn cli_names_the_working_directory_it_could_not_read() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    // A directory the child stands in and then unlinks, so hort runs with a
    // working directory the kernel no longer has and `getcwd` answers ENOENT.
    // Done in the child and never in the test process, because the whole suite
    // shares one process and a working directory is a property of it.
    let vanishing = TempDir::new().unwrap();
    let stood_in = CString::new(vanishing.path().as_os_str().as_bytes()).unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", &xdg_root).env("XDG_CONFIG_HOME", &config_home).arg("ls");
    unsafe {
        hort.pre_exec(move || {
            checked(libc::chdir(stood_in.as_ptr()))?;
            checked(libc::rmdir(stood_in.as_ptr()))
        });
    }

    let run = hort.output().unwrap();

    let message = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        !run.status.success(),
        "a run whose working directory is gone cannot get past assembling itself: {message}"
    );
    assert!(
        message.contains("working directory"),
        "the working directory is what hort could not read, and it is what the person has to go and fix: {message}"
    );
    assert!(
        !message.contains("state directory"),
        "and the state directory is neither where it failed nor derived from anything that did: {message}"
    );
    assert!(
        message.contains("could not read"),
        "a directory hort cannot read at all needs a different move from one it can read and not resolve, so the message says which happened: {message}"
    );
}

#[test]
fn cli_names_the_working_directory_it_could_not_resolve() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home("{}");
    // A directory whose parent the child closes to traversal once it is standing
    // inside it. `getcwd` still answers, off the kernel's own record of where the
    // process is, while resolving that same path opens it from the root and is
    // refused, which is what separates this arm from the one above.
    let closing = TempDir::new().unwrap();
    let closing_path = closing.path().canonicalize().unwrap();
    let inside = closing_path.join("inner");
    fs::create_dir(&inside).unwrap();
    let stood_in = CString::new(inside.as_os_str().as_bytes()).unwrap();
    let shut = CString::new(closing_path.as_os_str().as_bytes()).unwrap();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", &xdg_root).env("XDG_CONFIG_HOME", &config_home).arg("ls");
    unsafe {
        hort.pre_exec(move || {
            checked(libc::chdir(stood_in.as_ptr()))?;
            checked(libc::chmod(shut.as_ptr(), 0))
        });
    }

    let run = hort.output().unwrap();
    // Before any assertion, because a failed one leaves the rest of the test
    // unrun and the guard cannot take back a directory nobody may enter.
    fs::set_permissions(&closing_path, fs::Permissions::from_mode(0o700)).unwrap();

    let message = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        !run.status.success(),
        "a run whose working directory it cannot resolve cannot get past assembling itself: {message}"
    );
    assert!(
        message.contains("working directory"),
        "the working directory is what hort could not resolve, and it is what the person has to go and fix: {message}"
    );
    assert!(
        !message.contains("state directory"),
        "and the state directory is neither where it failed nor derived from anything that did: {message}"
    );
    assert!(
        message.contains("could not resolve"),
        "a directory hort can read and not resolve needs a different move from one it cannot read at all, so the message says which happened: {message}"
    );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_builds_a_sandbox_the_kernel_is_running() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // The one fact a placeholder runtime cannot produce. Everything before it,
    // the resolved config, the branch, the worktree and the record, exists just
    // the same in a build that never started a container.
    let record = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built");
    let anchor = record.liveness_token().expect("up records the anchor it started");
    assert!(Path::new(&format!("/proc/{}", anchor.pid.0)).exists());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_with_detach_returns_without_opening_a_session() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Nothing inside the box ever reads this, and that is the assertion: a build
    // that ignored the flag would run a shell that consumed it. Asked for by
    // whoever scripts hort, and a flag that quietly opens a session anyway is a
    // flag that lies.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .write_stdin("echo ran-inside-the-sandbox\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("ran-inside-the-sandbox").not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_without_detach_opens_a_session_in_the_sandbox_it_built() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The sandbox the user asked for is one they are standing in, and a build
    // that returns to the host prompt instead leaves them to work out that a box
    // exists somewhere and has to be entered by name.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("echo ran-inside-the-sandbox\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("ran-inside-the-sandbox"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// The file a session leaves in `/workdir` once it is running. `/workdir` is a
/// bind mount of a host directory, so what the session writes there is what the
/// test outside the sandbox reads to know the box has somebody in it.
const SESSION_WITNESS: &str = "session-is-open";

/// Whether `path` shows up before the deadline runs out. The whole build has to
/// finish and a shell has to start inside the box before anything can write
/// there, and none of it is done when the spawn returns.
fn appeared_within_deadline(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_ls_counts_the_session_open_in_a_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let witness = sandbox
        .state_dir()
        .join(format!("worktree-{}", sandbox.name().as_str()))
        .join(SESSION_WITNESS);

    // Spawned rather than run to completion, because the session has to still be
    // open when the next command asks about it: a shell whose input stays open is
    // the only way a second invocation of hort can find somebody in the box.
    let mut occupied = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"))
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut into_the_box = occupied.stdin.take().unwrap();
    writeln!(into_the_box, "touch /workdir/{SESSION_WITNESS}").unwrap();
    assert!(appeared_within_deadline(&witness), "no session ever reached the worktree");

    // Which probe the binary wires itself up with is settled where hort assembles
    // its adapters, and nothing below that wiring can witness it: every command
    // reads a probe that answers nothing as a box with nobody in it, so the whole
    // suite stays green while the column lies, `down` never asks before killing
    // somebody's work, and a box being typed in is offered to `prune --idle`.
    // Read as the cells of this sandbox's row and never as a spaced string: the
    // listing pads each column to its widest cell, and it lists every box the
    // host is running, so how wide a column is here is not this test's to know.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +live +1 ", sandbox.name().as_str()))
                .unwrap(),
        );

    drop(into_the_box);
    occupied.wait().unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_carries_a_declared_dotfile_into_the_sandbox_home() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_home, home_path) = temp_host_home();
    let declared = home_path.join(".config").join("hortwitness");
    fs::create_dir_all(&declared).unwrap();
    fs::write(declared.join("settings"), "carried-from-the-host\n").unwrap();
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "mounts": {{ "readOnly": ["{}"] }} }}"#,
        declared.display()
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Read back where the box looks for it, never where the host keeps it. A
    // build that lost the home it measures a declared path against mounts the
    // directory just the same and succeeds just the same, at the host's own
    // absolute path, an address nothing inside the box ever visits: the failure
    // is a sandbox that carries nothing and says so nowhere, and only the
    // destination tells the two apart. Which home and which roots reach the
    // adapters is settled where hort wires itself up, so nothing below that
    // wiring can witness this.
    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", &home_path)
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("cat /home/hort/.config/hortwitness/settings\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("carried-from-the-host"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", &home_path)
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_attach_exits_with_the_status_of_the_session() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Without this a script cannot tell a command that failed inside the box
    // from hort failing to open the box at all, and both of those are exit 1.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin("exit 7\n")
        .assert()
        .code(7);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_reports_a_configuration_advisory_on_stderr() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    fs::create_dir_all(repo_path.join(".devcontainer")).unwrap();
    fs::write(
        repo_path.join(".devcontainer").join("devcontainer.json"),
        r#"{ "image": "mcr.microsoft.com/devcontainers/base:bookworm" }"#,
    )
    .unwrap();
    let sandbox = ScratchSandbox::new();

    // Gated because only a build that succeeds reaches the advisory channel, and
    // the configuration half is the one worth the price: it is produced far from
    // the command that prints it, so dropping it is the silent failure.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .assert()
        .success()
        .stderr(predicate::str::contains("'image'"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_declared_cache_dir_survives_the_sandbox_that_wrote_it() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "cache": {{ "dirs": ["node_modules"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    // Worked out here rather than asked of hort, so what this reads back is an
    // address the test arrived at on its own and not one it was handed.
    let key = repo_path.display().to_string().replace('%', "%25").replace('/', "%2F");
    let cached =
        sandbox.state_root().join("cache").join(key).join("node_modules").join("installed");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("mkdir -p /workdir/node_modules && echo four-minutes-of-installing > /workdir/node_modules/installed\n")
        .assert()
        .success();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();

    // Read after the sandbox is gone, because outliving it is the entire point:
    // a box is disposable and what it took four minutes to install is not. Every
    // other suite can be green with this broken, since the address, the creation
    // of the directory, the writable bind and the wiring that carries them into
    // a real container are settled in four different places and only a run that
    // uses all four can say they agree.
    assert_eq!(fs::read_to_string(&cached).ok().as_deref(), Some("four-minutes-of-installing\n"));
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_writes_a_completion_into_the_channel_on_the_host() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "agents": [{{ "command": "claude", "notify": {{ "stopHook": true }} }}] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    // Declared after the fixtures it depends on, so it is the first of them to
    // go: the guard takes the sandbox down, and a container still standing over a
    // directory another guard has already removed is the failure the shutdown
    // order exists to prevent.
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("echo finished >> /run/hort/notify/events.jsonl\n")
        .assert()
        .success();

    // Read while the sandbox is still up, because `down` takes the channel away
    // with the rest of what remembers the sandbox. This is the crossing the whole
    // mechanism exists for: the hook runs inside a box that cannot reach the
    // host, and the only reason the host ever hears about a completion is that
    // this one directory is the same directory on both sides. Every layer of it
    // can be right on its own with the crossing broken.
    let events = sandbox.state_dir().join("notify").join("events.jsonl");
    assert_eq!(fs::read_to_string(&events).ok().as_deref(), Some("finished\n"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// A throwaway directory holding a stand-in for the host's desktop notification
/// program, plus the file it records into. Returned with its canonicalized path;
/// the `TempDir` guard must outlive the sandbox that raises through it.
///
/// It carries the real program's name, because what hort raises a notification
/// through is what it found on the search path, and this directory goes in front
/// of it.
fn temp_notify_sink() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let program = path.join("notify-send");
    let recording = path.join("raised");
    fs::write(&program, format!("#!/bin/sh\necho \"$@\" >> {}\n", recording.display())).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

/// The search path a sandbox is built with when `first` has to be found before
/// anything the host installed.
fn path_led_by(first: &Path) -> String {
    format!("{}:{}", first.display(), std::env::var("PATH").unwrap_or_default())
}

/// Whether `recording` comes to hold `expected` before the deadline runs out.
///
/// The watcher raises on its own time: the append happens inside the box, the
/// kernel wakes a host process about it, and that process runs a program. None of
/// that is finished when the command that made the sandbox returns.
fn raised_within_deadline(recording: &Path, expected: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read_to_string(recording).is_ok_and(|raised| raised.contains(expected)) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// Whether the process at `pid` is still the sandbox's watcher, asked the way the
/// teardown itself asks it.
///
/// A pid outlives the process it named, so the question is never "is something
/// there" but "is this still the helper that was recorded". A witness that invents
/// a second way to ask can drift from the one the code uses, and this family has
/// already produced one that could not tell a dead process from a live one.
fn names_the_watcher(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|name| name.trim() == "hort-watcher")
}

fn stopped_being_the_watcher_within_deadline(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !names_the_watcher(pid) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// Whether the process at `pid` is still a sandbox's pasta, asked the way the
/// network teardown itself asks it: by the program its command line names,
/// since a pid outlives the process it named and pasta's pid file outlives pasta.
fn names_pasta(pid: u32) -> bool {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else { return false };
    let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    Path::new(OsStr::from_bytes(program)).file_name() == Some(OsStr::new("pasta"))
}

fn stopped_being_pasta_within_deadline(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !names_pasta(pid) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

/// The pid this sandbox's pasta recorded itself under, read from the file hort
/// stops it by.
fn recorded_pasta_pid(sandbox: &ScratchSandbox) -> u32 {
    let recorded = fs::read_to_string(sandbox.sandbox_dir().join("pasta.pid"))
        .expect("the pid file of the sandbox's pasta");
    recorded.trim().parse().expect("a pid")
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_completion_inside_the_box_is_raised_on_the_host() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "agents": [{{ "command": "claude", "notify": {{ "stopHook": true }} }}] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let (_sink, sink_dir) = temp_notify_sink();
    // Declared after everything it raises through, so the guard stops the watcher
    // while the program that watcher runs is still on the machine.
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .env("PATH", path_led_by(&sink_dir))
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("echo '{\"event\":\"stop\"}' >> /run/hort/notify/events.jsonl\n")
        .assert()
        .success();

    // The whole chain in one assertion, and it is the only test that can tell
    // every layer being right from every layer being right with the crossing
    // broken: an agent finishing inside a box that cannot reach the host, the
    // append landing in a directory shared with it, the kernel waking a host
    // process about that directory, and that process raising a message naming the
    // sandbox through the program the host has for it. Each of those five is
    // settled somewhere else, and only a real run puts them in one line.
    assert!(raised_within_deadline(&sink_dir.join("raised"), sandbox.name().as_str()));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .env("PATH", path_led_by(&sink_dir))
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_runtime_root_holds_the_watcher_pid_file_until_the_sandbox_goes_down() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "agents": [{{ "command": "claude", "notify": {{ "stopHook": true }} }}] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let (_sink, sink_dir) = temp_notify_sink();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .env("PATH", path_led_by(&sink_dir))
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Read before the teardown, because the teardown takes the file with it. The
    // watcher is the fourth host-side process of this family and it is recorded
    // the way the other three are: beside them, under the root a restart empties,
    // and recognizable in the process table before anything signals it.
    let pid_file = sandbox.sandbox_dir().join("watcher.pid");
    let recorded = fs::read_to_string(&pid_file).expect("up records the watcher it started");
    let pid: u32 = recorded.trim().parse().expect("the recorded watcher is a pid");
    assert!(names_the_watcher(pid));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .env("PATH", path_led_by(&sink_dir))
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();

    // Both halves, because either one alone is satisfied by a teardown that did
    // half the job: a removed record with the process still holding the channel of
    // a sandbox that no longer exists, or a stopped process with a pid file that
    // outlives it and names a stranger on the next boot.
    assert!(!pid_file.exists());
    assert!(stopped_being_the_watcher_within_deadline(pid));
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_sees_the_dropped_hook_beside_what_only_the_base_carries() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "agents": [{{ "command": "claude", "notify": {{ "stopHook": true }} }}] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    // Declared last for the reason the type's own documentation gives: it has to
    // fall before the fixtures whose directories the container holds.
    let sandbox = ScratchSandbox::new();

    // Writing the drop-in means writing into `/etc` of the sandbox's own layer,
    // and that only works if the merge shows a directory living in both layers as
    // the union of the two. It is the expected behavior and hort leans on it
    // elsewhere, but if it shadows instead, the box's `/etc` becomes the one file
    // hort wrote and the whole base configuration disappears: no shell config, no
    // certificates, no users. Nothing in a suite that never boots a container can
    // see that, and the failure looks like the agent misbehaving.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("cat /etc/claude-code/managed-settings.d/hort-notify.json /etc/passwd\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""Stop""#))
        .stdout(predicate::str::contains("root:"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_down_without_git_leaves_the_project_folder() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let project = TempDir::new().unwrap();
    let project_path = project.path().canonicalize().unwrap();
    // No git here: the marker file is the whole of what makes this a project.
    fs::write(project_path.join(".hort.json"), "{}").unwrap();
    fs::write(project_path.join("notes.md"), "work in progress\n").unwrap();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&project_path)
        .args(["up", sandbox.name().as_str()])
        .assert()
        .success();

    let record = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built");
    assert_eq!(record.worktree_path(), project_path);
    assert_eq!(record.branch(), None);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&project_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();

    // The folder hort mounted is the user's own, and tearing the sandbox down
    // must leave it exactly where it was, contents and all.
    assert!(project_path.join("notes.md").exists());
    assert!(project_path.join(".hort.json").exists());
    assert!(!sandbox.state_dir().exists());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_runtime_root_holds_the_pasta_pid_file_until_the_sandbox_goes_down() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Which root reaches which adapter is decided once, where hort wires itself
    // up, and no test below that wiring can tell the two apart: an adapter asked
    // about the root it was handed answers the same either way. The two roots
    // point at different directories here for exactly that reason.
    assert!(sandbox.sandbox_dir().join("pasta.pid").exists());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();

    // The directory the pid file lived in is written by two owners that each
    // take away only their own files, and neither ever sweeps it: it goes when
    // the last file does. So an owner that forgot one of its files leaves the
    // whole directory standing, and this one line is what says every one of them
    // did its half.
    assert!(!sandbox.sandbox_dir().exists());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_writes_no_helper_artifact_into_the_state_root() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Writing these under both roots satisfies every other test here and keeps
    // the failure whole: the helpers that write them exec binaries the
    // distribution labels, and the label the state root carries refuses them, so
    // hort would still lose the pid it stops pasta by and the log it explains a
    // dead sandbox from.
    let state_dir = sandbox.state_dir();
    // Asked first, because two absences under a directory that does not exist are
    // two absences whatever hort did.
    assert!(state_dir.exists());
    assert!(!state_dir.join("pasta.pid").exists());
    assert!(!state_dir.join("output.log").exists());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
fn cli_prune_refuses_without_tty() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    write_orphaned_record(&state_root, "demo");
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .arg("prune")
        .assert()
        .code(1)
        .stderr(
            "refusing to prune without confirmation: stdin is not a TTY (pass --force to proceed)\n",
        );
}

#[test]
fn cli_prune_force_removes_orphaned_sandbox() {
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    let runtime = TempDir::new().unwrap();
    let runtime_root = runtime.path().canonicalize().unwrap();
    let (_repo, repo_path) = temp_git_repo();

    write_orphaned_record(&state_root, "demo");
    let worktree_path = state_root.join("sandboxes").join("demo").join("worktree-demo");
    git(&repo_path, &["worktree", "add", "-b", "demo", worktree_path.to_str().unwrap()]);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["prune", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"));

    assert!(!state_root.join("sandboxes").join("demo").exists());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_cannot_read_a_host_path_the_sandbox_never_declared() {
    let Some(rootfs) = prepared_rootfs() else { return };
    // Under the host's own home and never under `/tmp`: the box mounts a fresh
    // tmpfs at `/tmp`, so a file planted there would read as absent from a build
    // with no confinement whatsoever, and the witness would prove nothing.
    let (_home, home_path) = temp_host_home();
    let host_only = home_path.join("host-secret");
    fs::write(&host_only, "only-the-host-can-read-this\n").unwrap();
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The `echo` is the control. Without it a session that never opened at all
    // would satisfy the absence below by producing no output whatsoever.
    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", &home_path)
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!("cat {}\necho the-session-ran\n", host_only.display()))
        .assert()
        .success()
        .stdout(predicate::str::contains("the-session-ran"))
        .stdout(predicate::str::contains("only-the-host-can-read-this").not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("HOME", &home_path)
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_climbing_out_of_the_worktree_reaches_the_container_root() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Listed rather than climbed with `cd`, because the kernel has to be the one
    // that walks `..`: a shell resolves `cd /workdir/..` against the string it
    // keeps and answers `/` without ever asking what the parent of a mount is.
    // `metadata.json` sits in the host directory the worktree lives in, so it is
    // what a climb that landed on the host would have listed instead.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("ls -a /workdir/..\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("workdir"))
        .stdout(predicate::str::contains("metadata.json").not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_runs_in_a_mount_namespace_of_its_own() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let host_namespace = fs::read_link("/proc/self/ns/mnt").unwrap().display().to_string();
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The mechanism the other two cannot pin. A plain `chroot` hides the host
    // filesystem and stops `..` at the new root exactly the same way, and what
    // tells the two apart is which mount namespace the session is running in.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("readlink /proc/self/ns/mnt\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("mnt:["))
        .stdout(predicate::str::contains(host_namespace).not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// What a throwaway repository still answers on the host, so a test can state
/// what survived as a literal.
fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = GitCommand::new("git").current_dir(dir).args(args).output().unwrap();
    let complaint = String::from_utf8_lossy(&output.stderr).trim().to_string();
    assert!(output.status.success(), "git {args:?} failed: {complaint}");
    String::from_utf8(output.stdout).unwrap()
}

/// A throwaway git repository with one commit on `main`, under the user's real
/// home rather than under `/tmp`, returned with its canonicalized path.
///
/// The sandbox mounts a fresh tmpfs at `/tmp`, so a repository planted there
/// reads as absent from inside a box that confines nothing whatsoever, and a
/// witness built on that absence passes on a broken build. The `TempDir` guard
/// must outlive the test.
fn temp_git_repo_under_the_host_home() -> (TempDir, PathBuf) {
    let real_home = std::env::home_dir().expect("the test runner has a home directory");
    let dir = TempDir::new_in(real_home).unwrap();
    let path = dir.path().canonicalize().unwrap();
    git(&path, &["init", "-b", "main"]);
    fs::write(path.join("README.md"), "seed\n").unwrap();
    git(&path, &["add", "README.md"]);
    git(
        &path,
        &[
            "-c",
            "user.name=hort tests",
            "-c",
            "user.email=tests@hort.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    (dir, path)
}

/// Take every ordinary entry of the worktree with it, then list what is left.
///
/// The `find -exec rm -rf` reaches even a name that starts with a dot, so it
/// aims at the worktree's `.git` too. That file is now a read-only bind
/// mountpoint, and without CAP_SYS_ADMIN the box cannot unlink a mountpoint, so
/// the `rm` is refused with `Resource busy` and the pointer to the real
/// repository survives while everything else under the worktree is gone. The
/// trailing `find` prints its own starting point and one line per surviving
/// entry, so a wiped worktree prints exactly `/workdir` and `/workdir/.git`.
const DESTROY_THE_WORKTREE: &str = "find /workdir -mindepth 1 -exec rm -rf {} +\nfind /workdir\n";

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_host_repository_keeps_its_history_after_the_worktree_is_destroyed() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(DESTROY_THE_WORKTREE)
        .assert()
        .success()
        .stdout(predicate::str::contains("/workdir/.git"))
        .stdout(predicate::function(|out: &str| {
            out.lines().filter(|line| line.starts_with("/workdir/")).eq(["/workdir/.git"])
        }));

    assert_eq!(git_output(&repo_path, &["log", "--format=%s", "main"]), "initial\n");
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_branch_the_sandbox_never_touched_survives_the_destruction() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    git(&repo_path, &["config", "user.name", "hort tests"]);
    git(&repo_path, &["config", "user.email", "tests@hort.invalid"]);
    git(&repo_path, &["checkout", "-q", "-b", "keeper"]);
    fs::write(repo_path.join("keeper.txt"), "work the sandbox never sees\n").unwrap();
    git(&repo_path, &["add", "keeper.txt"]);
    git(&repo_path, &["commit", "-q", "-m", "on the keeper branch"]);
    git(&repo_path, &["checkout", "-q", "main"]);
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(DESTROY_THE_WORKTREE)
        .assert()
        .success()
        .stdout(predicate::str::contains("/workdir/.git"))
        .stdout(predicate::function(|out: &str| {
            out.lines().filter(|line| line.starts_with("/workdir/")).eq(["/workdir/.git"])
        }));

    assert_eq!(
        git_output(&repo_path, &["show", "keeper:keeper.txt"]),
        "work the sandbox never sees\n"
    );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_cannot_read_the_repository_its_worktree_points_at() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo_under_the_host_home();
    let sandbox = ScratchSandbox::new();

    // The worktree's `.git` is a pointer file naming a host path, so the box is
    // handed the address of the real repository and the guarantee is that the
    // address buys it nothing. The `echo` is the control: without it a session
    // that died on the read would satisfy the absence below by printing nothing.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!(
            "cat /workdir/.git\ncat {}/.git/HEAD\necho the-session-ran\n",
            repo_path.display()
        ))
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("gitdir: {}", repo_path.display())))
        .stdout(predicate::str::contains("ref: refs/heads/main").not())
        .stdout(predicate::str::contains("the-session-ran"));
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_ls_reports_orphaned_after_the_anchor_is_killed() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    let record = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built");
    // Whoever hort itself named as the anchor, so that killing something else
    // cannot make the listing below say what this test wants to hear.
    let anchor = record.liveness_token().expect("up records the anchor it started");
    let signalled = unsafe { libc::kill(anchor.pid.0 as libc::pid_t, libc::SIGKILL) };
    assert_eq!(signalled, 0, "the anchor could not be signalled, so nothing was killed");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +orphaned +0 ", sandbox.name().as_str()))
                .unwrap(),
        )
        .stdout(
            predicate::str::is_match(format!("(?m) {} +clean *$", sandbox.name().as_str()))
                .unwrap(),
        );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_ls_reports_inconsistent_when_the_worktree_is_deleted_under_a_live_box() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    // Taking a directory away from a container that has it bound is the one
    // thing the order of a shutdown exists to forbid, and it is the arrangement
    // on purpose: what a file manager can do to a live box is to be reported and
    // never prevented. What is destroyed lives inside the guard's own state
    // home, so the blast radius is this fixture.
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));
    fs::remove_dir_all(&worktree).unwrap();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +inconsistent +0 ", sandbox.name().as_str()))
                .unwrap(),
        )
        .stdout(
            predicate::str::is_match(format!("(?m) {} +- *$", sandbox.name().as_str())).unwrap(),
        );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_ls_reports_lost_record_when_the_metadata_of_a_live_box_is_removed() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    // The file and never the directory holding it: the directory carries the
    // worktree too, and a box missing both answers for two kinds of damage at
    // once, which is a different question from the one asked here.
    fs::remove_file(sandbox.state_dir().join("metadata.json")).unwrap();

    // Every cell past the session count is a dash, however many columns the
    // listing has: what a box with no record can show is this test's question,
    // and which columns a row carries is the renderer's.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!(
                "(?m)^{} +lost-record +0( +-)+ *$",
                sandbox.name().as_str()
            ))
            .unwrap(),
        );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_attach_refuses_a_sandbox_whose_container_state_vanished() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    // The runtime's own bookkeeping of the container and nothing else. The
    // anchor keeps standing, so the box is alive; the record and the worktree
    // stay where they are, so this is the one arrangement in which every other
    // command answers for the box and only entering it is refused. It lives
    // under the guard's runtime root, so the blast radius is this fixture, and
    // the guard collects the box afterwards by signal, as `down` does when the
    // state is gone.
    fs::remove_dir_all(sandbox.runtime_root().join("containers").join(sandbox.name().as_str()))
        .unwrap();
    // The path hort itself recorded, so the message is held to naming the
    // worktree the record names and not one the test derived on its own.
    let record = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built");
    let name = sandbox.name().as_str();
    let worktree = record.worktree_path().display();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", name])
        .assert()
        .code(1)
        .stderr(format!(
            "sandbox '{name}' is running but its container state is gone, so no session can join it (commit what you want to keep from {worktree} on the host, then run 'hort down {name}' and 'hort up {name} --branch {name}')\n"
        ));
}

/// Run `hort up -d` on `sandbox` and kill it the instant the record it writes
/// is on disk, with `SIGKILL` to its own pid and to nothing else, so that what
/// is left is what an abrupt death at that point leaves and not what the
/// children it had by then would have done on being signalled themselves.
///
/// The record is what is watched for, never the anchor, because the record
/// coming first is the guarantee itself: a run that ends before writing one has
/// broken that order in the other direction, and its own words say why; one
/// that neither ends nor writes within the deadline is reported as that rather
/// than waited on, since "no record before the anchor" is the failure this
/// exists to catch. What stood when the signal landed is asserted here, before
/// the product is asked anything, because a signal that arrives after hort has
/// released its anchor leaves a different state, one the listing reads the same
/// way, and a verdict read off that state would be measuring luck.
fn killed_once_its_record_is_written(sandbox: &ScratchSandbox, config_home: &Path, repo: &Path) {
    let mut up = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"))
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(repo)
        .args(["up", "-d", sandbox.name().as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let record = sandbox.state_dir().join("metadata.json");
    let deadline = Instant::now() + ANSWERED_BY;
    while !record.exists() {
        if let Some(ended) = up.try_wait().unwrap() {
            let mut said = String::new();
            up.stderr.take().unwrap().read_to_string(&mut said).unwrap();
            panic!("up ended ({ended}) before it wrote a record: {said}");
        }
        assert!(Instant::now() < deadline, "up wrote no record in {ANSWERED_BY:?}");
        std::thread::yield_now();
    }
    up.kill().unwrap();
    let ended = up.wait().unwrap();

    assert_eq!(
        ended.signal(),
        Some(libc::SIGKILL),
        "up ended on its own ({ended}) before the signal reached it"
    );
    let recorded = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("the record up was killed on is there to read");
    assert_eq!(
        recorded.liveness_token(),
        None,
        "the record up was killed on already named an anchor"
    );
    assert!(
        !a_process_declares(sandbox.name()),
        "an anchor stood after the kill: the signal landed after up had released it"
    );
}

/// Whether some process on this host says it belongs to the sandbox `name`, by
/// the marker hort exports into everything it starts inside a box. Asked of the
/// kernel, the way hort's own reconciliation asks it, because the listing
/// cannot tell a record whose anchor never came up from one whose anchor came
/// up after hort had stopped writing.
fn a_process_declares(name: &SandboxName) -> bool {
    let marker = format!("HORT_SANDBOX={}", name.as_str());
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|process| fs::read(process.path().join("environ")).ok())
        .any(|environ| environ.split(|byte| *byte == 0).any(|entry| entry == marker.as_bytes()))
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_ls_reports_orphaned_after_up_was_killed_once_its_record_was_written() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    killed_once_its_record_is_written(&sandbox, &config_home, &repo_path);

    // The tail is the rest of the row: the branch and the dirty probe are read
    // off a worktree the killed run had already made, and a half-built record
    // that broke either would put a dash there while the verdict still read
    // `orphaned`.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +orphaned +0 ", sandbox.name().as_str()))
                .unwrap(),
        )
        .stdout(
            predicate::str::is_match(format!("(?m) {} +clean *$", sandbox.name().as_str()))
                .unwrap(),
        );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_completes_a_sandbox_whose_up_was_killed_once_its_record_was_written() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    killed_once_its_record_is_written(&sandbox, &config_home, &repo_path);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Exit 0 alone is satisfied by a run that refused nothing and built
    // nothing; the listing is what says the box the first run left half-built
    // is now standing.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +live +0 ", sandbox.name().as_str()))
                .unwrap(),
        )
        .stdout(
            predicate::str::is_match(format!("(?m) {} +clean *$", sandbox.name().as_str()))
                .unwrap(),
        );
}

/// Whether the anchor `anchor` names stopped being alive, asked the way hort's
/// own reconciliation asks it, within a deadline a killed process has no reason
/// to need.
fn stopped_being_the_anchor_within_deadline(anchor: &LivenessToken) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !ProcLivenessProbe.is_alive(anchor) {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_completes_a_sandbox_whose_anchor_was_killed() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    let anchor = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built")
        .liveness_token()
        .expect("up records the anchor it started");
    let signalled = unsafe { libc::kill(anchor.pid.0 as libc::pid_t, libc::SIGKILL) };
    assert_eq!(signalled, 0, "the anchor could not be signalled, so nothing was killed");
    assert!(
        stopped_being_the_anchor_within_deadline(&anchor),
        "the anchor was still alive after the kill, so the run below would meet a live box"
    );
    // What separates this from the box a reboot leaves: the runtime root is
    // wiped by a reboot and survives a kill, so a run that found it gone would
    // be completing the easier arrangement and saying nothing about this one.
    assert!(
        sandbox.runtime_root().join("containers").join(sandbox.name().as_str()).is_dir(),
        "the runtime kept no state of the killed container, so this is not the arrangement a kill leaves"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Exit 0 alone is satisfied by a run that refused nothing and built
    // nothing; the listing is what says the box the killed anchor left behind
    // is standing again.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .arg("ls")
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(format!("(?m)^{} +live +0 ", sandbox.name().as_str()))
                .unwrap(),
        );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_brings_the_network_of_a_live_sandbox_back() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();
    // The arm that already ships, a helper dying under a live box: pasta
    // handles the signal and leaves cleanly, its pid file stays behind naming a
    // process that is gone, and the anchor stands on. The box is live by every
    // measure the kernel offers and reaches nothing, which is the state an `up`
    // killed between the anchor and the network leaves without any signal to
    // pasta at all.
    let pasta = recorded_pasta_pid(&sandbox);
    let signalled = unsafe { libc::kill(pasta as libc::pid_t, libc::SIGTERM) };
    assert_eq!(signalled, 0, "pasta could not be signalled, so nothing was killed");
    assert!(
        stopped_being_pasta_within_deadline(pasta),
        "pasta {pasta} was still running after the signal, so the arrangement was not produced"
    );
    let anchor = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built")
        .liveness_token()
        .expect("up records the anchor it started");
    assert!(
        ProcLivenessProbe.is_alive(&anchor),
        "the anchor did not survive its pasta, so the run below would resume an orphan and not a live box"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Exit 0 alone is satisfied by a run that refused nothing and wired
    // nothing; the file hort stops pasta by naming a running pasta again is
    // what says the box carries traffic once more.
    let revived = recorded_pasta_pid(&sandbox);
    assert!(names_pasta(revived), "pasta.pid names {revived}, which is not a running pasta");
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_refuses_a_branch_a_down_kept_without_offering_it_when_stdin_is_not_a_terminal() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let name = sandbox.name().as_str();
    // What a `down` leaves of a sandbox: its branch, and no worktree or record.
    git(&repo_path, &["branch", name]);
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", name]);

    let transcript = output_only_terminal(hort);

    // The whole of what reached the terminal, so a question put there fails
    // this. Only stdin is kept off the terminal, because a prompt looks at
    // stderr: with every stream piped, a question asked anyway would refuse on
    // its own and hand back this very sentence.
    assert_eq!(
        transcript,
        format!(
            "branch '{name}' already exists (a 'hort down' keeps a sandbox's branch) — run 'hort up {name} --branch {name}' to build the sandbox on it, or choose another name\r\n"
        )
    );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_builds_on_a_branch_a_down_kept_when_the_offer_is_taken_at_a_terminal() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let name = sandbox.name().as_str();
    // A commit only the kept branch carries, so a worktree cut from HEAD instead
    // is told apart from one on the branch by what is in it.
    git(&repo_path, &["checkout", "-q", "-b", name]);
    fs::write(repo_path.join("kept-by-the-branch.txt"), "kept\n").unwrap();
    git(&repo_path, &["add", "kept-by-the-branch.txt"]);
    git(
        &repo_path,
        &[
            "-c",
            "user.name=hort tests",
            "-c",
            "user.email=tests@hort.invalid",
            "commit",
            "-q",
            "-m",
            "work a down kept",
        ],
    );
    git(&repo_path, &["checkout", "-q", "main"]);
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", name]);

    // Enter alone, because taking the offer is what the question defaults to.
    let (how_it_ended, transcript) = ended_at_the_terminal(hort, "\r");

    assert_eq!(how_it_ended.code(), Some(0), "the offer taken builds the sandbox: {transcript}");
    assert!(
        sandbox
            .state_dir()
            .join(format!("worktree-{name}"))
            .join("kept-by-the-branch.txt")
            .exists(),
        "on the branch that was kept, holding the work committed there: {transcript}"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", name])
        .assert()
        .success();
}

/// The file a session writes into the merged root, and the bytes that say the
/// write landed. A read back is `cat`, which complains to stderr about a file
/// that is not there, so the content is the only thing that can reach stdout.
const MERGED_ROOT_FILE: &str = "written-into-the-merged-root";
const EPHEMERAL_WRITE: &str = "this-write-is-ephemeral";

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_write_to_the_container_root_does_not_survive_down() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!(
            "echo {EPHEMERAL_WRITE} > /{MERGED_ROOT_FILE}\ncat /{MERGED_ROOT_FILE}\n"
        ))
        .assert()
        .success()
        .stdout(predicate::str::contains(EPHEMERAL_WRITE));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();

    // The same name again, because the name is what a sandbox's state is filed
    // under and a different one would be asking about a different box. The
    // branch the first one made is still in the repository afterwards, and
    // reusing it is the way hort's own collision message tells a user to. The
    // `echo` is the control: without it a session that never opened would
    // satisfy the absence below by printing nothing at all.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str(), "--branch", sandbox.name().as_str()])
        .write_stdin(format!("cat /{MERGED_ROOT_FILE}\necho the-second-box-ran\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("the-second-box-ran"))
        .stdout(predicate::str::contains(EPHEMERAL_WRITE).not());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_write_to_the_container_root_never_reaches_the_base_rootfs() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // `/etc/passwd` is the control, and it is what makes the absence on the host
    // mean anything: it is carried by the base and by nothing else hort mounts,
    // so reading it proves the directory inspected below is the layer this write
    // landed on top of. Without it a box with no base at all would satisfy the
    // assertion.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!(
            "echo {EPHEMERAL_WRITE} > /{MERGED_ROOT_FILE}\ncat /{MERGED_ROOT_FILE} /etc/passwd\n"
        ))
        .assert()
        .success()
        .stdout(predicate::str::contains(EPHEMERAL_WRITE))
        .stdout(predicate::str::contains("root:"));

    assert!(!Path::new(&rootfs).join(MERGED_ROOT_FILE).exists());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_write_in_one_sandbox_is_invisible_to_another_on_the_same_base() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    // Two boxes under one state root, because that is what a person running two
    // of them has and the writable layer of each is a directory under it: two
    // sandboxes meeting in one of those is the leak this asks about, and a state
    // root each would rule it out by arrangement instead of measuring it. The
    // runtime root stays per box, since a container's own state lives there and
    // each guard stops the box whose name it holds. The guard owning the shared
    // state root is declared first so it falls last, or the directory the second
    // box is standing in is taken away while it is still running.
    let writer = ScratchSandbox::new();
    let reader = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", writer.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", writer.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", writer.name().as_str()])
        .write_stdin(format!(
            "echo {EPHEMERAL_WRITE} > /{MERGED_ROOT_FILE}\ncat /{MERGED_ROOT_FILE}\n"
        ))
        .assert()
        .success()
        .stdout(predicate::str::contains(EPHEMERAL_WRITE));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", writer.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", reader.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", reader.name().as_str()])
        .write_stdin(format!("cat /{MERGED_ROOT_FILE}\ncat /etc/passwd\necho the-second-box-ran\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("the-second-box-ran"))
        .stdout(predicate::str::contains("root:"))
        .stdout(predicate::str::contains(EPHEMERAL_WRITE).not());
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_write_to_the_worktree_is_on_the_host_afterwards() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin("echo this-write-is-kept > /workdir/kept-by-the-worktree\n")
        .assert()
        .success();

    // Read with the sandbox still standing, which is the whole window this
    // guarantee has: the worktree is what a person commits from, and `down`
    // takes it away on purpose once they have.
    assert_eq!(
        fs::read_to_string(worktree.join("kept-by-the-worktree")).ok().as_deref(),
        Some("this-write-is-kept\n")
    );
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_cannot_rewrite_the_worktree_git_pointer() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));

    // The worktree's `.git` is a pointer file naming the host repository, and a
    // box that rewrites it makes a later host `git` in this worktree read
    // configuration the box controls, which runs as the user outside every
    // layer hort has. The session overwrites the pointer with a marker of its
    // own; the sibling write is the control, because a pointer that stays put
    // only proves the guarantee if an ordinary write to `/workdir` still lands.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(
            "echo planted-by-the-agent > /workdir/.git\necho this-write-is-kept > /workdir/kept-by-the-worktree\n",
        )
        .assert()
        .success();

    // Read from the host with the box still standing, which is exactly where and
    // when a person commits: the genuine pointer starts `gitdir:`, so its
    // survival is the read-only bind refusing the rewrite.
    let pointer = fs::read_to_string(worktree.join(".git")).unwrap();
    assert!(
        pointer.starts_with("gitdir:"),
        "the box rewrote the worktree's .git pointer, so a host git here would run planted config: {pointer:?}"
    );
    // Without the control write landing, a still-genuine pointer would prove
    // nothing: it could be a box that refused every write rather than one that
    // refused only the read-only `.git`.
    assert_eq!(
        fs::read_to_string(worktree.join("kept-by-the-worktree")).ok().as_deref(),
        Some("this-write-is-kept\n"),
        "the sibling write never landed, so a genuine pointer discriminates nothing"
    );
}

/// The two ends of one terminal: the side a person types into, and the side
/// hort is run on.
fn a_terminal() -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    let opened = unsafe {
        libc::openpty(&mut master, &mut slave, ptr::null_mut(), ptr::null(), ptr::null())
    };
    assert_eq!(opened, 0, "opening a terminal to run hort on");
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// What hort wrote to a terminal it was given for its output alone, with stdin
/// coming from somewhere that is not one.
///
/// The arrangement is the point: `dialoguer` guards itself by looking at stderr
/// while hort decides by looking at stdin, so this is where the two can disagree
/// and where what hort read is observable at all.
///
/// The run gets a session of its own, which is what keeps a build that decides
/// to ask something anyway off the keyboard of whoever runs the suite: with no
/// controlling terminal there is no `/dev/tty` for a prompt to fall back to, so
/// it fails at once instead of waiting, and that is also why this one needs no
/// deadline.
fn output_only_terminal(mut hort: std::process::Command) -> String {
    let (master, slave) = a_terminal();
    hort.stdin(Stdio::null()).stdout(slave.try_clone().unwrap()).stderr(slave.try_clone().unwrap());
    unsafe {
        hort.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut running = hort.spawn().unwrap();
    drop(slave);
    drop(hort);

    let mut terminal = fs::File::from(master);
    let mut transcript = Vec::new();
    let _ = terminal.read_to_end(&mut transcript);
    running.wait().unwrap();
    String::from_utf8_lossy(&transcript).into_owned()
}

/// What came back over the terminal after `keys` was typed at a run of `hort`
/// held on one.
///
/// Separate from the session reader below, and the difference is the deadline:
/// this drives a build that is expected to stop and ask, so a build that asks
/// something these keys do not answer would hold the terminal open forever and
/// take the suite with it. Killing on the deadline turns that into a red test.
fn typed_at_the_terminal(mut hort: std::process::Command, keys: &str) -> String {
    let (master, slave) = a_terminal();
    hort.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    let mut running = hort.spawn().unwrap();
    drop(slave);
    drop(hort);

    let mut terminal = fs::File::from(master);
    terminal.write_all(keys.as_bytes()).unwrap();

    let waiting_on = running.id();
    let ended = Arc::new(AtomicBool::new(false));
    let watched = ended.clone();
    let deadline = std::thread::spawn(move || {
        let expires = Instant::now() + ANSWERED_BY;
        while Instant::now() < expires {
            if watched.load(Ordering::Relaxed) {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        // Asked once more on the way out, because the run may have ended in the
        // last interval: the flag is what stops the kill from landing on
        // whatever the host gave that number to next.
        if !watched.load(Ordering::Relaxed) {
            unsafe { libc::kill(waiting_on as i32, libc::SIGKILL) };
        }
    });

    let mut transcript = Vec::new();
    let _ = terminal.read_to_end(&mut transcript);
    running.wait().unwrap();
    ended.store(true, Ordering::Relaxed);
    deadline.join().unwrap();
    String::from_utf8_lossy(&transcript).into_owned()
}

/// How a run of `hort` held on a terminal ended, and what the terminal saw on
/// the way, after `keys` was typed at it.
///
/// Separate from the transcript reader above because the question is a different
/// one: there the terminal is read for what hort said, here for whether the run
/// reached an end of its own. A build that asks one question more than these
/// keys answer never gets to a status, so the deadline kills it and what comes
/// back is a signal, which no run that finished ever ends with.
///
/// The run gets a session of its own, for the reason `output_only_terminal`
/// takes one: with no controlling terminal there is no `/dev/tty` left to fall
/// back to, so a build that goes looking for somewhere else to ask fails here
/// instead of reading the keyboard of whoever is running the suite.
fn ended_at_the_terminal(
    mut hort: std::process::Command,
    keys: &str,
) -> (std::process::ExitStatus, String) {
    let (master, slave) = a_terminal();
    hort.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    unsafe {
        hort.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut running = hort.spawn().unwrap();
    drop(slave);
    drop(hort);

    let mut terminal = fs::File::from(master);
    terminal.write_all(keys.as_bytes()).unwrap();

    let waiting_on = running.id();
    let ended = Arc::new(AtomicBool::new(false));
    let watched = ended.clone();
    let deadline = std::thread::spawn(move || {
        let expires = Instant::now() + ANSWERED_BY;
        while Instant::now() < expires {
            if watched.load(Ordering::Relaxed) {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        // Asked once more on the way out, because the run may have ended in the
        // last interval: the flag is what stops the kill from landing on
        // whatever the host gave that number to next.
        if !watched.load(Ordering::Relaxed) {
            unsafe { libc::kill(waiting_on as i32, libc::SIGKILL) };
        }
    });

    let mut transcript = Vec::new();
    let _ = terminal.read_to_end(&mut transcript);
    let how_it_ended = running.wait().unwrap();
    ended.store(true, Ordering::Relaxed);
    deadline.join().unwrap();
    (how_it_ended, String::from_utf8_lossy(&transcript).into_owned())
}

/// What came back over the terminal after `script` was typed into the session
/// `hort` opened on it.
///
/// The terminal is the arrangement, not a detail of it. hort allocates a pty for
/// a session only when it was invoked on one, and a session that asks for a
/// terminal is the one thing the runtime takes only as a process file, which
/// replaces what the tenant builder would otherwise have assembled rather than
/// adding to it. A script piped in lands in a session assembled somewhere else,
/// carrying whatever that other place puts on it.
fn typed_into_a_session(mut hort: std::process::Command, script: &str) -> String {
    let (master, slave) = a_terminal();
    hort.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    let mut running = hort.spawn().unwrap();
    // The read below ends when the last holder of the session's side of the
    // terminal is gone, and hort has to be that holder. A command keeps its own
    // copy of every stream it was handed for as long as the command itself is
    // around, so letting it go is as load-bearing as letting this one go.
    drop(slave);
    drop(hort);

    let mut terminal = fs::File::from(master);
    terminal.write_all(script.as_bytes()).unwrap();
    let mut transcript = Vec::new();
    // A terminal nobody holds the other side of any more is reported as a
    // failure rather than as end of input, so the read that fills the transcript
    // is also the read that fails.
    let _ = terminal.read_to_end(&mut transcript);
    running.wait().unwrap();

    String::from_utf8_lossy(&transcript).into_owned()
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_shell_a_session_opens_holds_no_capability() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()]);

    // The shell's own pid and never `/proc/self`, which is the `cat` it started:
    // the kernel builds a child's permitted and effective sets out of its
    // parent's ambient one at the exec, so a shell holding a capability it never
    // made ambient reads back as clean through anything it runs. The anchor is
    // another process in the same namespace, and it is not the one anybody is
    // typing into.
    let transcript = typed_into_a_session(hort, "cat /proc/$$/status\nexit\n");

    // All five sets and not the effective one alone. A process whose effective
    // set is empty while its bounding set is not is one raise away from holding
    // the capability, which is the whole of what an empty capability set is for.
    assert!(transcript.contains("CapInh:\t0000000000000000"));
    assert!(transcript.contains("CapPrm:\t0000000000000000"));
    assert!(transcript.contains("CapEff:\t0000000000000000"));
    assert!(transcript.contains("CapBnd:\t0000000000000000"));
    assert!(transcript.contains("CapAmb:\t0000000000000000"));
}

/// How often the watch below asks. Small enough that the gap it is looking for
/// cannot fall between two samples: a teardown that took the worktree out from
/// under a live container would leave it gone for as long as stopping that
/// container takes, which is milliseconds, and this asks dozens of times in that
/// span.
const SAMPLE: Duration = Duration::from_micros(200);

/// What an outside observer saw of a sandbox while it was being torn down.
struct WhatTheTeardownLookedLike {
    stood_on_its_worktree: bool,
    went_together: bool,
    lost_its_worktree_first: bool,
}

/// Watch a sandbox from the host while `tear_down` runs, asking about the anchor
/// and about its worktree in the same sample so their order can be read off the
/// samples rather than inferred.
///
/// The state before is read here rather than in the watch, so that whether the
/// box was standing does not depend on the first sample winning a race against
/// the command. The anchor is asked the way hort asks it, through the liveness
/// token it recorded, so a killed process the kernel has not reaped yet reads
/// dead rather than alive. The watch runs on for a moment after the teardown
/// returns, because the state it left behind is the last thing this has to see.
fn watched_while<T>(
    worktree: &Path,
    anchor: LivenessToken,
    tear_down: impl FnOnce() -> T,
) -> (WhatTheTeardownLookedLike, T) {
    let stood_on_its_worktree = ProcLivenessProbe.is_alive(&anchor) && worktree.exists();
    let stop = Arc::new(AtomicBool::new(false));
    let watching = stop.clone();
    let watched = worktree.to_path_buf();
    let observer = std::thread::spawn(move || {
        let mut went_together = false;
        let mut lost_its_worktree_first = false;
        while !watching.load(Ordering::Relaxed) {
            let anchor_alive = ProcLivenessProbe.is_alive(&anchor);
            let worktree_there = watched.exists();
            went_together |= !anchor_alive && !worktree_there;
            lost_its_worktree_first |= anchor_alive && !worktree_there;
            sleep(SAMPLE);
        }
        (went_together, lost_its_worktree_first)
    });

    let outcome = tear_down();
    sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let (went_together, lost_its_worktree_first) = observer.join().unwrap();

    (
        WhatTheTeardownLookedLike { stood_on_its_worktree, went_together, lost_its_worktree_first },
        outcome,
    )
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_worktree_is_removed_only_after_the_container_that_held_it_is_gone() {
    let Some(rootfs) = prepared_rootfs() else { return };
    // An agent that announces it finished is what puts a watcher on the host, so
    // this box comes down with the whole family standing: the anchor, a session,
    // pasta and the watcher.
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "agents": [{{ "command": "claude", "notify": {{ "stopHook": true }} }}] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let (_sink, sink_dir) = temp_notify_sink();
    let sandbox = ScratchSandbox::new();
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));
    let witness = worktree.join(SESSION_WITNESS);

    // Spawned with its input held open, so somebody is still inside the box when
    // it is torn down. The order exists for the case where a process is holding
    // the mounted folder, and a box nobody is in never puts it to the test.
    let mut occupied = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"))
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .env("PATH", path_led_by(&sink_dir))
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut into_the_box = occupied.stdin.take().unwrap();
    writeln!(into_the_box, "touch /workdir/{SESSION_WITNESS}").unwrap();
    assert!(appeared_within_deadline(&witness), "no session ever reached the worktree");

    let anchor = FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built")
        .liveness_token()
        .expect("up records the anchor it started");

    let (seen, torn_down) = watched_while(&worktree, anchor, || {
        std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"))
            .env("XDG_STATE_HOME", sandbox.state_home())
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
            .env("PATH", path_led_by(&sink_dir))
            .current_dir(&repo_path)
            .args(["down", "--force", sandbox.name().as_str()])
            .output()
            .unwrap()
    });

    drop(into_the_box);
    occupied.wait().unwrap();

    assert!(torn_down.status.success(), "{}", String::from_utf8_lossy(&torn_down.stderr));
    // The two the watch positively saw come first: an observer that never looked
    // reports the forbidden state missing exactly as an observer that looked and
    // never found it. Deleting a folder a process still has mounted is what
    // corrupts I/O, so the whole guarantee is that the worktree was never once
    // gone while the anchor holding it was still alive.
    assert!(seen.stood_on_its_worktree);
    assert!(seen.went_together);
    assert!(!seen.lost_its_worktree_first);
}

/// The public host the egress witnesses that need one are written against, in
/// both postures: what an allowlist admits, and, where there is no allowlist at
/// all, simply a host out there that answers.
///
/// A stand-in on this machine would not serve. What is under test is hort's
/// proxy being handed a name, resolving that name itself, reading the name the
/// connection then asks for, and splicing the two ends together, and only a host
/// that really answers puts all of that to work.
const ALLOWLISTED_HOST: &str = "api.anthropic.com";

/// A host the allowlist deliberately leaves out.
const OMITTED_HOST: &str = "example.com";

/// An address out on the internet, for the probes that have to find no way to
/// it. Nothing needs to answer there for those to mean what they say.
const AN_ADDRESS_OUT_THERE: &str = "1.1.1.1";

/// Where this machine reaches [`ALLOWLISTED_HOST`] right now, or `None` after
/// saying why, so a run with no route to the internet reports that rather than
/// failing over something that is not hort's doing.
///
/// Over IPv4, because that is the family the rest of this is written against and
/// letting the other one in would put a second variable in the answer. Skipping
/// costs something and it is worth naming: a skip is a pass nobody hears, so the
/// count of these lines in a gated run is checked the same way the count of
/// failures is.
fn a_reachable_public_host() -> Option<SocketAddr> {
    let Ok(addresses) = format!("{ALLOWLISTED_HOST}:443").to_socket_addrs() else {
        eprintln!(
            "skipped: this machine does not resolve {ALLOWLISTED_HOST}, so it has no public host to test against"
        );
        return None;
    };
    for address in addresses.filter(SocketAddr::is_ipv4) {
        if TcpStream::connect_timeout(&address, Duration::from_secs(5)).is_ok() {
            return Some(address);
        }
    }
    eprintln!(
        "skipped: this machine does not reach {ALLOWLISTED_HOST} on 443 over IPv4, so it has no public host to test against"
    );
    None
}

/// What the declared database below answers with.
const DATABASE_BANNER: &str = "the-declared-database-answered\n";

/// A service answering [`DATABASE_BANNER`] on a host loopback port the kernel
/// picks, given back so a project can declare it. The listener is never let go:
/// the port a sandbox is told to reach a database on is the port the host has to
/// still be listening on when the sandbox dials it.
fn a_host_service_on_loopback() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut dialled in listener.incoming().flatten() {
            let _ = dialled.write_all(DATABASE_BANNER.as_bytes());
        }
    });
    port
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_in_an_open_sandbox_reaches_a_public_host() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let Some(public) = a_reachable_public_host() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Addressed and not named, which is a measured constraint rather than a
    // softer question: a prepared rootfs carries no resolver configuration and
    // hort writes none, so nothing inside an open box turns a name into an
    // address. What open mode promises is that what leaves is unfiltered, and a
    // connection landing on a host out on the internet is that promise kept.
    // The status is what is read, never the quiet around it: a shell that cannot
    // find the tool answers 127, and nothing should ever take that for a
    // connection that was stopped.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!("nc -w 5 -z {} {}\necho reached=$?\n", public.ip(), public.port()))
        .assert()
        .success()
        .stdout(predicate::str::contains("reached=0\n"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_in_an_open_sandbox_resolves_a_public_name() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let Some(public) = a_reachable_public_host() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The name is the one the allowlist witnesses use, because the guard above
    // resolves exactly that host and a second name would leave this unguarded.
    // Nothing here is allowlisted: this box admits everything.
    //
    // Two dials of the same host, one addressed and one named, so a red says
    // which half failed instead of leaving it to be guessed: the addressed line
    // is what the sibling witness above already holds, and the named line is the
    // whole of what this one adds. Both statuses are anchored at the end of the
    // line, because a shell that cannot find the tool answers 127 and nothing
    // here should ever read that as an answer about the network.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!(
            "nc -w 5 -z {} {}\necho addressed=$?\nnc -w 5 -z {ALLOWLISTED_HOST} 443\necho named=$?\n",
            public.ip(),
            public.port()
        ))
        .assert()
        .success()
        .stdout(predicate::str::contains("addressed=0\n"))
        .stdout(predicate::str::contains("named=0\n"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_an_open_sandbox_runs_no_proxy() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let helpers = sandbox.sandbox_dir();
    // pasta before the proxy, and that order is the whole of what makes the two
    // lines under it mean anything: a file missing from a directory nothing ever
    // wrote to is missing for a reason that has nothing to do with the posture.
    // A helper hort cannot record is a helper hort kills, so the sandbox having
    // no record of a proxy is the sandbox having no proxy.
    assert!(helpers.join("pasta.pid").exists(), "an open sandbox is still wired by pasta");
    assert!(!helpers.join("proxy.pid").exists(), "an open sandbox started a proxy");
    assert!(!helpers.join("proxy.port").exists(), "an open sandbox published a proxy port");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_an_allowlisted_host_answers_through_the_proxy() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let Some(_reachable) = a_reachable_public_host() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));

    // The hello is made by the box's own TLS library rather than carried in
    // ready-made, so nothing here can go stale against what the host out there
    // will still accept. It travels through a file because the request and the
    // hello have to arrive on one connection: the proxy admits the name on the
    // CONNECT line, then holds the name inside the hello against it, and a hello
    // sent on a second connection would be held against nothing. The address
    // dialled is the one hort itself put in the environment, which is where a
    // tool inside the box reads it.
    let tunnel = format!(
        "ssl_client -n {ALLOWLISTED_HOST} < /dev/null > /workdir/hello.bin 2>/dev/null\n\
         {{ printf 'CONNECT {ALLOWLISTED_HOST}:443 HTTP/1.1\\r\\n\\r\\n'; cat /workdir/hello.bin; sleep 2; }} | nc -w 5 127.0.0.1 ${{HTTPS_PROXY##*:}} > /workdir/answer.bin\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(tunnel)
        .assert()
        .success();

    let answer = fs::read(worktree.join("answer.bin")).expect("the session opened no tunnel");
    let opened = b"HTTP/1.1 200 Connection Established\r\n\r\n";
    assert!(answer.starts_with(opened), "{}", String::from_utf8_lossy(&answer));
    // Being let through is only half of it. The tunnel is opened before the
    // connection has said which host it is really for, so a box that got this
    // far and no further has been told nothing about whether anything out there
    // answered it. A handshake record at the version every server still writes
    // is the far end starting to talk, and nothing on this side of the splice
    // could have put it there.
    assert!(answer[opened.len()..].starts_with(&[0x16, 0x03, 0x03]), "nothing answered");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_host_the_allowlist_omits_is_refused_by_the_proxy() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let worktree = sandbox.state_dir().join(format!("worktree-{}", sandbox.name().as_str()));

    // Nothing follows the request, because the allowlist is judged on the name
    // the request carries and a host it does not name never gets as far as being
    // asked for a hello.
    let refused = format!(
        "{{ printf 'CONNECT {OMITTED_HOST}:443 HTTP/1.1\\r\\n\\r\\n'; sleep 1; }} | nc -w 5 127.0.0.1 ${{HTTPS_PROXY##*:}} > /workdir/refusal.txt\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(refused)
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(worktree.join("refusal.txt"))
            .expect("the session asked the proxy nothing"),
        "HTTP/1.1 403 Forbidden\r\n\r\n"
    );
    // Refused, and refused for the one reason the allowlist exists for. The very
    // same 403 answers a request the proxy could make no sense of, so without
    // the line the proxy wrote about this one, a build that turned everything
    // away would read exactly like a build that reads the list.
    let decisions = fs::read_to_string(sandbox.sandbox_dir().join("output.log")).unwrap();
    assert!(
        decisions.contains(&format!("refused {OMITTED_HOST} (not in the allowlist)")),
        "{decisions}"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_an_allowlisted_sandbox_has_no_route_of_its_own() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The status carries its own line ending into every one of these, because a
    // status is read by matching text and `1` is the opening of `127`: without
    // the break after it, the answer a shell gives for a tool it never found
    // reads as the answer it gives for a connection that was stopped, which is
    // the one confusion all of this is written to avoid.
    //
    // Three probes and one guarantee: a stream, a datagram and a lookup all fail
    // for the same missing thing, which is any way out of the namespace at all.
    // They are read by status and not by silence, because a shell that cannot
    // find the tool answers 127 and prints nothing either. The first line is the
    // control that makes the other three readable: the same tool, dialling the
    // one address this sandbox does have a way to, has to succeed, or all this
    // says is that nothing in the box works. The lookup names its own reason on
    // the way out, and no missing applet ever says that; it says it on the
    // stream a complaint goes to, so that one is folded into the other before
    // anything reads it.
    let probes = format!(
        "nc -w 5 -z 127.0.0.1 ${{HTTPS_PROXY##*:}}\necho control=$?\n\
         nc -w 5 -z {AN_ADDRESS_OUT_THERE} 443\necho raw=$?\n\
         echo out | nc -u -w 3 {AN_ADDRESS_OUT_THERE} 53\necho udp=$?\n\
         nslookup {OMITTED_HOST} {AN_ADDRESS_OUT_THERE} 2>&1\necho dns=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(probes)
        .assert()
        .success()
        .stdout(predicate::str::contains("control=0\n"))
        .stdout(predicate::str::contains("raw=1\n"))
        .stdout(predicate::str::contains("udp=1\n"))
        .stdout(predicate::str::contains("dns=1\n"))
        .stdout(predicate::str::contains("Network unreachable"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_declared_database_answers_inside_an_allowlisted_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let database = a_host_service_on_loopback();
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }},
              "network": [ {{ "mode": "host", "host": "127.0.0.1", "port": {database} }} ] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Declared on the host's loopback and reached on the sandbox's own, on the
    // same number, which is the one address a declared database ever has inside
    // a box. What is asserted is the line the service answers with: a tool the
    // box does not have could not have printed it, and neither could a sandbox
    // that reached nothing.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!("nc -w 5 127.0.0.1 {database} < /dev/null\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains(DATABASE_BANNER.trim()));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// A host loopback port nothing listens on yet, for a service that has to come
/// up after the sandbox declaring it was built. The kernel picks it and it is let
/// go at once, so the one assumption is that nothing takes it in between.
///
/// Picked by the kernel is load-bearing: it comes from the range the kernel hands
/// out on its own, which pasta, left to scan for ports by itself, never forwards.
/// A declared database on such a port is reached only because hort told pasta
/// about it, which is the case a project whose database the kernel placed there
/// actually has.
fn a_loopback_port_nothing_listens_on() -> u16 {
    TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

/// A service answering [`DATABASE_BANNER`] on this host loopback port from now
/// on, never let go, like [`a_host_service_on_loopback`].
fn a_host_service_on_loopback_port(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("the declared port is still free");
    std::thread::spawn(move || {
        for mut dialled in listener.incoming().flatten() {
            let _ = dialled.write_all(DATABASE_BANNER.as_bytes());
        }
    });
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_declared_database_answers_inside_an_open_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let database = a_loopback_port_nothing_listens_on();
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}",
              "network": [ {{ "mode": "host", "host": "127.0.0.1", "port": {database} }} ] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // The database comes up only once the sandbox is standing, which is how a
    // project starts its services in practice, so what the box reaches cannot
    // depend on what happened to be listening on the host when it was built.
    // The address is the one a declared database has in both postures, and the
    // banner is what only that service says, so a transcript carrying it says the
    // declaration, and nothing else, is what reached it. The status is echoed
    // because this nc says nothing when a connection is refused, and a
    // transcript that fails has to say which way it failed.
    a_host_service_on_loopback_port(database);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "sh", "-c"])
        .arg(format!("nc -w 5 127.0.0.1 {database} < /dev/null; echo dialled=$?"))
        .assert()
        .success()
        .stdout(predicate::str::contains(DATABASE_BANNER.trim()));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// A service answering `banner` on a host loopback port below the range the
/// kernel hands out on its own, given back so a project can leave it undeclared.
///
/// Below that range is load-bearing. pasta, left to scan for ports by itself,
/// forwards only ports outside it, so an undeclared service on a port the kernel
/// picked is refused inside the box whatever hort told pasta, and a probe
/// against one could not tell a sandbox that reaches what nobody declared from
/// one that does not. The range assumes the kernel's own starts above it, which
/// is its default.
fn a_host_service_below_the_ephemeral_range(banner: &'static str) -> u16 {
    let listener = (20000..21000)
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("a free host loopback port below the ephemeral range");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut dialled in listener.incoming().flatten() {
            let _ = dialled.write_all(banner.as_bytes());
        }
    });
    port
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_host_loopback_port_an_open_sandbox_did_not_declare_is_refused() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let undeclared = a_host_service_below_the_ephemeral_range(UNDECLARED_BANNER);
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // The sandbox's own loopback means what the project declared and nothing
    // else, in this posture as in the other, so a service the project never
    // named is not there even though open egress filters nothing on the way
    // out. The status tells a refusal from a tool that is missing, and the
    // banner is what only that service says, so the transcript not carrying it
    // is the claim: nothing inside the box got to it.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(format!("nc -w 5 127.0.0.1 {undeclared} < /dev/null\necho undeclared=$?\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("undeclared=1\n"))
        .stdout(predicate::str::contains(UNDECLARED_BANNER.trim()).not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// A banner nothing on this machine but the undeclared service below answers
/// with, so a transcript that carries it says where it came from.
const UNDECLARED_BANNER: &str = "the-undeclared-service-answered\n";

/// A service answering `banner` on a host loopback port the kernel picks, given
/// back so a project can leave it undeclared. Like the declared one it is never
/// let go: a port with nothing behind it is refused for a reason that has
/// nothing to do with what the sandbox was wired for, and a probe against that
/// says nothing.
fn a_host_service_answering(banner: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut dialled in listener.incoming().flatten() {
            let _ = dialled.write_all(banner.as_bytes());
        }
    });
    port
}

/// Where this machine reaches [`ALLOWLISTED_HOST`] over IPv6 right now, or
/// `None` after saying why.
///
/// The address is only half of what this is for. What the witness below measures
/// is a route that is not there, and on a machine with no IPv6 of its own there
/// would have been no route to take away, so the probe would answer the same on
/// a build that empties one address family and on a build that empties both.
fn a_reachable_public_host_over_ipv6() -> Option<SocketAddr> {
    let Ok(addresses) = format!("{ALLOWLISTED_HOST}:443").to_socket_addrs() else {
        eprintln!(
            "skipped: this machine does not resolve {ALLOWLISTED_HOST}, so it has no public host to test against"
        );
        return None;
    };
    for address in addresses.filter(SocketAddr::is_ipv6) {
        if TcpStream::connect_timeout(&address, Duration::from_secs(5)).is_ok() {
            return Some(address);
        }
    }
    eprintln!(
        "skipped: this machine does not reach {ALLOWLISTED_HOST} on 443 over IPv6, so it has no IPv6 route for a sandbox to be denied"
    );
    None
}

/// The port an allowlisted sandbox's own proxy answers on, taken from what the
/// run that wired the sandbox wrote down.
///
/// Every probe that has to tell "the kernel would not route this" from "the
/// sandbox may not touch this port" needs a port the sandbox may touch, and this
/// is the one every allowlisted sandbox has.
fn proxy_port_of(sandbox_dir: &Path) -> u16 {
    fs::read_to_string(sandbox_dir.join("proxy.port"))
        .expect("an allowlisted sandbox records the port its proxy answers on")
        .trim()
        .parse()
        .expect("the recorded proxy port is a port")
}

/// The address pasta gave the sandbox's namespace as its router, read off the
/// report pasta writes into the sandbox's own log.
///
/// There is nowhere else to ask. The routing table inside an allowlisted box is
/// empty by design, and the address is one hort takes from the host and records
/// nowhere. pasta names the IPv4 router before the IPv6 one, and reading the
/// answer as an address is what makes a report that ever changes shape fail here
/// loudly instead of quietly aiming a probe at nothing.
fn gateway_reported_to(sandbox_dir: &Path) -> std::net::Ipv4Addr {
    fs::read_to_string(sandbox_dir.join("output.log"))
        .expect("pasta reports the topology it configured")
        .lines()
        .find_map(|line| line.trim().strip_prefix("router: "))
        .expect("pasta names the router of the namespace it configured")
        .parse()
        .expect("the router pasta named is an address")
}

/// The network namespace a process on this host is in, as the kernel names it.
fn network_namespace_of(pid: u32) -> String {
    fs::read_link(format!("/proc/{pid}/ns/net"))
        .expect("a live process has a network namespace")
        .to_string_lossy()
        .into_owned()
}

/// The anchor `up` recorded for this sandbox, asked of the record hort wrote.
fn anchor_of(sandbox: &ScratchSandbox) -> u32 {
    FileMetadataStore::new(sandbox.state_root())
        .get(sandbox.name())
        .unwrap()
        .expect("up records the sandbox it built")
        .liveness_token()
        .expect("up records the anchor it started")
        .pid
        .0
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_closed_netns_is_closed_over_ipv6_too() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let Some(public) = a_reachable_public_host_over_ipv6() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let proxy = proxy_port_of(&sandbox.sandbox_dir());
    let address = public.ip();
    // Three dials of one tool with one set of flags, so that what separates the
    // answers is the address and the port and nothing else. The first is the
    // control: the sandbox's own proxy, on the sandbox's own loopback, has to
    // answer, or every silence below is the silence of a box where nothing
    // works. The second is the claim itself, the public host over IPv6. The
    // third is the same host on the one port this sandbox is allowed to touch,
    // and it is there because of what the second one cannot say.
    let probes = format!(
        "nc -v -w 5 127.0.0.1 {proxy} < /dev/null 2>&1\necho control=$?\n\
         nc -v -w 8 {address} 443 < /dev/null 2>&1\necho v6=$?\n\
         nc -v -w 8 {address} {proxy} < /dev/null 2>&1\necho v6permitted=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin(probes)
        .assert()
        .success()
        // Every status carries its own line ending, because a status is read by
        // matching text and `1` opens `127`, which is what a shell answers for a
        // tool it never found.
        .stdout(predicate::str::contains(format!("(127.0.0.1:{proxy}) open")))
        .stdout(predicate::str::contains("control=0\n"))
        .stdout(predicate::str::contains("v6=1\n"))
        .stdout(predicate::str::contains("v6permitted=1\n"))
        // The address named, and named as unroutable. The line above says the
        // sandbox got nowhere; this one says why, and it is the only one of the
        // two that a build which stopped emptying the IPv6 table would stop
        // producing. Without it the pair reads the same either way, because the
        // port restriction the session runs under refuses 443 before the kernel
        // is ever asked for a route.
        .stdout(predicate::str::contains(format!("[{address}]:{proxy}): Network unreachable")));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_host_loopback_port_the_sandbox_did_not_declare_is_refused() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let declared = a_host_service_on_loopback();
    // A service and not an empty port. A port with nothing behind it is turned
    // away for a reason that has nothing to do with how the sandbox was wired,
    // so a probe against one would report a closed door on a build that left the
    // whole of the host's loopback open.
    let undeclared = a_host_service_answering(UNDECLARED_BANNER);
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }},
              "network": [ {{ "mode": "host", "host": "127.0.0.1", "port": {declared} }} ] }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    // Two ports of one host loopback, dialled from inside with one tool and one
    // set of flags, so the declaration is the only thing that differs between
    // them. The first has to answer: it is what says the tool is there, that the
    // sandbox does reach the host's loopback where it was told to, and that a
    // banner crossing that way is a thing this transcript can carry at all.
    let probes = format!(
        "nc -v -w 5 127.0.0.1 {declared} < /dev/null 2>&1\necho declared=$?\n\
         nc -v -w 5 127.0.0.1 {undeclared} < /dev/null 2>&1\necho undeclared=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", sandbox.name().as_str()])
        .write_stdin(probes)
        .assert()
        .success()
        .stdout(predicate::str::contains(DATABASE_BANNER.trim()))
        .stdout(predicate::str::contains("declared=0\n"))
        .stdout(predicate::str::contains("undeclared=1\n"))
        // What the undeclared service says is a thing only the undeclared
        // service says, so the transcript not carrying it is the whole claim:
        // nothing inside the box got to it.
        .stdout(predicate::str::contains(UNDECLARED_BANNER.trim()).not());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_gateway_address_answers_no_port() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let proxy = proxy_port_of(&sandbox.sandbox_dir());
    let gateway = gateway_reported_to(&sandbox.sandbox_dir());
    // The port is the one the sandbox is allowed to touch, and it is the whole
    // point of the pair. On any other port the answer would be the port
    // restriction talking, which says nothing about the address; on this one the
    // restriction stands aside and the only thing left to refuse is the kernel
    // having no way to that address. So the two lines differ in the address and
    // in nothing else, and one of them has to work.
    let probes = format!(
        "nc -v -w 5 127.0.0.1 {proxy} < /dev/null 2>&1\necho loopback=$?\n\
         nc -v -w 5 {gateway} {proxy} < /dev/null 2>&1\necho gateway=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin(probes)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("(127.0.0.1:{proxy}) open")))
        .stdout(predicate::str::contains("loopback=0\n"))
        .stdout(predicate::str::contains("gateway=1\n"))
        .stdout(predicate::str::contains(format!("{gateway}:{proxy}): Network unreachable")));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_session_lives_in_the_network_namespace_of_its_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // Read from the host, because the session cannot read it: the anchor sits in
    // a user namespace the session is not in, so from inside the box the
    // anchor's namespace links come back as permission denied. The host is where
    // both are visible at once.
    let anchor = network_namespace_of(anchor_of(&sandbox));
    // The one arrangement this whole question exists for is a session that
    // joined the namespace of whoever opened it instead of the one the sandbox
    // was built with. Were the sandbox never given a namespace of its own, the
    // two would agree while the box sat wide open on the host's, so what the
    // sandbox has has to differ from what this process has before the agreement
    // below means anything.
    assert_ne!(anchor, network_namespace_of(std::process::id()));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin("readlink /proc/self/ns/net\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(anchor));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_session_a_terminal_opens_lives_in_the_network_namespace_of_its_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let anchor = network_namespace_of(anchor_of(&sandbox));
    assert_ne!(anchor, network_namespace_of(std::process::id()));

    // The same question asked of the other kind of session, and it is a
    // different program being asked. A session that wants a terminal is handed
    // to the runtime as a process file that takes the place of what the tenant
    // builder would have put together, so whatever the piped one inherits it
    // inherits somewhere else. The two were already measured to land in one
    // cgroup, which says nothing at all about which network namespace either is
    // in.
    let mut hort = std::process::Command::new(assert_cmd::cargo::cargo_bin("hort"));
    hort.env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()]);

    let transcript = typed_into_a_session(hort, "readlink /proc/self/ns/net\nexit\n");

    // The terminal echoes back what was typed into it, and what was typed names
    // no namespace, so the only way this string reaches the transcript is the
    // session having answered with it.
    assert!(transcript.contains(&anchor), "{transcript}");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_the_agent_cannot_add_a_route_out_of_its_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let gateway = gateway_reported_to(&sandbox.sandbox_dir());
    // The first line is the premise and not decoration: what this measures is
    // that being root buys nothing here, and on a build where the session ran as
    // an ordinary user the refusal below would be the refusal any user gets and
    // would say nothing about who owns the namespace. The second is the control,
    // and it is the same program doing the harmless half of the same job, so a
    // rootfs without it fails there rather than reading as a route that could
    // not be added.
    let attempt = format!(
        "echo uid=$(id -u)\n\
         ip route show 2>&1\necho read=$?\n\
         ip route add default via {gateway} 2>&1\necho add=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin(attempt)
        .assert()
        .success()
        .stdout(predicate::str::contains("uid=0\n"))
        .stdout(predicate::str::contains("read=0\n"))
        .stdout(predicate::str::contains("add=2\n"))
        // The kernel's own words for it, which is what separates a namespace the
        // agent may not configure from every other reason a route might not take.
        .stdout(predicate::str::contains("Operation not permitted"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_raw_connection_still_fails_after_the_agent_tried_to_open_a_route() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(
        r#"{{ "rootfs": "{rootfs}", "egress": {{ "allow": ["{ALLOWLISTED_HOST}"] }} }}"#
    ));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    let proxy = proxy_port_of(&sandbox.sandbox_dir());
    let gateway = gateway_reported_to(&sandbox.sandbox_dir());
    // The attempt first, and it has to have been made, or what follows is the
    // ordinary run of an untouched box. Then the control, then the raw dial the
    // agent would make. The last line is what says the attempt bought nothing at
    // the level it was aimed at: on the one port the session may touch, the
    // kernel still has no way out, which is the same answer this box gave before
    // anybody reached for the routing table.
    let after = format!(
        "ip route add default via {gateway} 2>&1\necho add=$?\n\
         nc -v -w 5 127.0.0.1 {proxy} < /dev/null 2>&1\necho control=$?\n\
         nc -v -w 5 {AN_ADDRESS_OUT_THERE} 443 < /dev/null 2>&1\necho raw=$?\n\
         nc -v -w 5 {AN_ADDRESS_OUT_THERE} {proxy} < /dev/null 2>&1\necho routed=$?\n"
    );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["attach", sandbox.name().as_str()])
        .write_stdin(after)
        .assert()
        .success()
        .stdout(predicate::str::contains("add=2\n"))
        .stdout(predicate::str::contains(format!("(127.0.0.1:{proxy}) open")))
        .stdout(predicate::str::contains("control=0\n"))
        .stdout(predicate::str::contains("raw=1\n"))
        .stdout(predicate::str::contains("routed=1\n"))
        .stdout(predicate::str::contains(format!(
            "{AN_ADDRESS_OUT_THERE}:{proxy}): Network unreachable"
        )));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_run_executes_the_command_inside_the_sandbox() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // run is the sibling of attach: it joins the live box and execs the command
    // the caller named instead of a login shell, and the command's own stdout
    // comes back on hort's. An orchestrator on the host drives a box this way.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "echo", "ran-inside-the-sandbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran-inside-the-sandbox"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_run_propagates_a_nonzero_exit_code() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // hort leaves with the command's own status, which is the whole point of run
    // for a script: a command that failed inside the box has to be tellable from
    // hort failing to open the box, and a build that returned zero regardless
    // would read every failure as a success.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "sh", "-c", "exit 7"])
        .assert()
        .code(7);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_run_reports_a_signalled_command_as_128_plus_the_signal() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", sandbox.name().as_str()])
        .assert()
        .success();

    // The exit byte of a signalled command's wait status is zero, so without the
    // shell's 128+signal rule a command the kernel killed reads to a script as
    // one that finished its work. SIGKILL is uncatchable, so 137 is the same
    // number on every run rather than something the command could trap away.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "sh", "-c", "kill -KILL $$"])
        .assert()
        .code(137);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

/// The prepared rootfs, when it carries a git a session can actually run.
///
/// Asked of the directory entry and never of what the entry points at: every
/// program in a busybox rootfs is a symlink to an absolute path that resolves
/// only inside the box, so a question that follows the link answers "absent"
/// for every one of them.
fn rootfs_carrying_git() -> Option<String> {
    let rootfs = prepared_rootfs()?;
    let root = Path::new(&rootfs);
    if fs::symlink_metadata(root.join("usr/bin/git")).is_ok()
        || fs::symlink_metadata(root.join("bin/git")).is_ok()
    {
        return Some(rootfs);
    }
    eprintln!("skipped: the prepared rootfs carries no git, so no session in it can run one");
    None
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_clone_mode_sandbox_can_write_the_git_directory_of_its_own_clone() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", "--git", "clone", sandbox.name().as_str()])
        .assert()
        .success();

    // In clone mode `/workdir/.git` is the box's own repository directory, and
    // every commit the agent makes writes into it. The read-only bind that a
    // worktree's pointer file carries would refuse all of it, so in this mode
    // the bind is gone and the directory answers a write.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "touch", "/workdir/.git/written-inside"])
        .assert()
        .success();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_a_clone_mode_sandbox_reads_the_host_object_store_it_cannot_write() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", "--git", "clone", sandbox.name().as_str()])
        .assert()
        .success();

    // The clone borrows every object it did not make from the host store, at the
    // one address it recorded inside itself, so the box finds no commit at all
    // without this mount. Every git repository keeps a `pack` directory under
    // its objects, so a listing naming it is what tells "lent" from "not there".
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", sandbox.name().as_str(), "--", "ls", GIT_OBJECTS])
        .assert()
        .success()
        .stdout(predicate::str::contains("pack"));

    // And what is lent is the user's real history, which nothing in the box may
    // rewrite. The listing above is what makes this refusal attributable: a
    // write to an address nothing is mounted at is refused too, and for a
    // reason that would leave the guarantee unmeasured.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args([
            "run",
            sandbox.name().as_str(),
            "--",
            "touch",
            &format!("{GIT_OBJECTS}/written-inside"),
        ])
        .assert()
        .failure();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs carrying git (HORT_TEST_ROOTFS) and pasta"]
fn cli_an_agent_commits_inside_a_clone_mode_sandbox_and_the_host_repository_is_unchanged() {
    let Some(rootfs) = rootfs_carrying_git() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    let sandbox = ScratchSandbox::new();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", "--git", "clone", sandbox.name().as_str()])
        .assert()
        .success();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args([
            "run",
            sandbox.name().as_str(),
            "--",
            "sh",
            "-c",
            "echo 'work the agent did' >> /workdir/README.md",
        ])
        .assert()
        .success();

    // The whole point of clone mode, and the one thing no spike measured: git
    // itself, running under the seccomp profile hort gives every process of a
    // sandbox, reading borrowed objects through a read-only mount and writing
    // its own into the box. A commit exercises all three at once.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args([
            "run",
            sandbox.name().as_str(),
            "--",
            "git",
            "-c",
            "user.name=hort agent",
            "-c",
            "user.email=agent@hort.invalid",
            "commit",
            "-am",
            "committed from inside the box",
        ])
        .assert()
        .success();

    // The host repository lent its objects and nothing else: the commit the box
    // made lives in the box's own clone, and the branch the user works on here
    // reads exactly as it did before the sandbox existed.
    assert_eq!(git_output(&repo_path, &["log", "--format=%s", "main"]), "initial\n");

    // That same commit now exists nowhere but in the box, and `down` will not
    // destroy work like that unless it is told to. The commit is a fixture and
    // the guarantee above is already measured, so this teardown gives it up on
    // purpose.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["down", "--force", sandbox.name().as_str()])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs carrying git (HORT_TEST_ROOTFS) and pasta"]
fn cli_down_without_a_terminal_refuses_to_destroy_a_commit_only_the_clone_holds() {
    let Some(rootfs) = rootfs_carrying_git() else { return };
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    // Where both teardowns below are run from: another project's repository,
    // which is where somebody cleaning up sandboxes is as likely to be standing
    // as in this one. A check that asked the repository it runs in, instead of
    // the project the sandbox was built from, never finds the returned commit
    // there and keeps refusing.
    let (_elsewhere, another_project) = temp_git_repo();
    let sandbox = ScratchSandbox::new();
    let name = sandbox.name().as_str();
    let bundle = sandbox.state_dir().join(format!("worktree-{name}")).join("work.bundle");

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["up", "-d", "--git", "clone", name])
        .assert()
        .success();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", name, "--", "sh", "-c", "echo 'work the agent did' >> /workdir/README.md"])
        .assert()
        .success();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args([
            "run",
            name,
            "--",
            "git",
            "-c",
            "user.name=hort agent",
            "-c",
            "user.email=agent@hort.invalid",
            "commit",
            "-am",
            "committed from inside the box",
        ])
        .assert()
        .success();

    // The commit exists in the clone and nowhere else, so tearing the box down
    // would leave it existing nowhere at all. There is no terminal here to ask
    // at, so `down` has to refuse, and the commit has to still be where the
    // agent left it once it has. Reading it back through `run` also says the
    // box is still running, because `run` refuses one that is not.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&another_project)
        .args(["down", name])
        .assert()
        .code(1)
        .stderr(
            "refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)\n",
        );

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args(["run", name, "--", "git", "-C", "/workdir", "log", "-1", "--format=%s"])
        .assert()
        .success()
        .stdout("committed from inside the box\n");

    // Those words, byte for byte, are also what an open session gets, and what
    // a clone gets when hort cannot tell whether the host repository has its
    // commits. On their own they would pass on a build that never looks for the
    // commit at all, as long as one of those two refused first. Taking the work
    // out to the host repository, through a bundle made inside the box where the
    // borrowed history resolves, changes the one fact this refusal is about and
    // neither of the others, so the same `down` going through afterwards is what
    // says the first one stopped for the commit.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&repo_path)
        .args([
            "run",
            name,
            "--",
            "git",
            "-C",
            "/workdir",
            "bundle",
            "create",
            "/workdir/work.bundle",
            name,
        ])
        .assert()
        .success();
    git(&repo_path, &["fetch", bundle.to_str().unwrap(), &format!("{name}:refs/hort/incoming")]);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", sandbox.state_home())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", sandbox.runtime_dir())
        .current_dir(&another_project)
        .args(["down", name])
        .assert()
        .success();
}
