//! End-to-end CLI tests: drive the built `hort` binary and assert on its exit
//! code, stdout, and stderr. Each test points the binary at a throwaway state
//! root (via `XDG_STATE_HOME`) and a throwaway git repository, so the real user
//! state is never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as GitCommand;

use assert_cmd::Command;
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
    let (_repo, repo_path) = temp_git_repo();

    write_orphaned_record(&state_root, "demo");
    let worktree_path = state_root.join("sandboxes").join("demo").join("worktree-demo");
    git(&repo_path, &["worktree", "add", "-b", "demo", worktree_path.to_str().unwrap()]);

    Command::cargo_bin("hort")
        .unwrap()
        .env("XDG_STATE_HOME", &xdg_root)
        .current_dir(&repo_path)
        .args(["down", "demo"])
        .assert()
        .success();

    assert!(!worktree_path.exists());
    assert!(!state_root.join("sandboxes").join("demo").exists());
}
