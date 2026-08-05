//! End-to-end CLI tests: drive the built `hort` binary and assert on its exit
//! code, stdout, and stderr. Each test points the binary at a throwaway state
//! root (via `XDG_STATE_HOME`) and a throwaway git repository, so the real user
//! state is never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as GitCommand;

use assert_cmd::Command;
use hort::adapters::metadata::FileMetadataStore;
use hort::domain::model::SandboxName;
use hort::ports::MetadataStore;
use predicates::prelude::*;
use tempfile::TempDir;

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

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_builds_a_sandbox_the_kernel_is_running() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let state_root = xdg_root.join("hort");
    let runtime = TempDir::new().unwrap();
    let runtime_root = runtime.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["up", "clidemo"])
        .assert()
        .success();

    // The one fact a placeholder runtime cannot produce. Everything before it,
    // the resolved config, the branch, the worktree and the record, exists just
    // the same in a build that never started a container.
    let record = FileMetadataStore::new(state_root.clone())
        .get(&SandboxName::new("clidemo").unwrap())
        .unwrap()
        .expect("up records the sandbox it built");
    let anchor = record.liveness_token().expect("up records the anchor it started");
    assert!(Path::new(&format!("/proc/{}", anchor.pid.0)).exists());

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["down", "clidemo"])
        .assert()
        .success();
}

#[test]
#[ignore = "needs unprivileged user namespaces, a prepared rootfs (HORT_TEST_ROOTFS) and pasta"]
fn cli_up_reports_a_configuration_advisory_on_stderr() {
    let Some(rootfs) = prepared_rootfs() else { return };
    let xdg = TempDir::new().unwrap();
    let xdg_root = xdg.path().canonicalize().unwrap();
    let runtime = TempDir::new().unwrap();
    let runtime_root = runtime.path().canonicalize().unwrap();
    let (_config, config_home) = temp_config_home(&format!(r#"{{ "rootfs": "{rootfs}" }}"#));
    let (_repo, repo_path) = temp_git_repo();
    fs::create_dir_all(repo_path.join(".devcontainer")).unwrap();
    fs::write(
        repo_path.join(".devcontainer").join("devcontainer.json"),
        r#"{ "image": "mcr.microsoft.com/devcontainers/base:bookworm" }"#,
    )
    .unwrap();

    // Gated because only a build that succeeds reaches the advisory channel, and
    // the configuration half is the one worth the price: it is produced far from
    // the command that prints it, so dropping it is the silent failure.
    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["up", "cfgdemo"])
        .assert()
        .success()
        .stderr(predicate::str::contains("'image'"));

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .current_dir(&repo_path)
        .args(["down", "cfgdemo"])
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
