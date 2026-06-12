//! `GitWorktreeProvider`: the git-backed `WorktreeProvider`. Shells the system
//! `git` (never a git library) to add, list, remove, and inspect the worktrees
//! that back each sandbox's `/workdir`, one per sandbox at
//! `<state_root>/sandboxes/<name>/worktree-<name>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName};
use crate::ports::{Worktree, WorktreeProvider};

/// A `WorktreeProvider` backed by the system `git`, rooted at the project
/// repository and the hort state directory the worktrees live under.
pub struct GitWorktreeProvider {
    repo_dir: PathBuf,
    state_root: PathBuf,
}

impl GitWorktreeProvider {
    /// Build a provider for the git repository at `repo_dir`, placing worktrees
    /// under `state_root`.
    pub fn new(repo_dir: PathBuf, state_root: PathBuf) -> Self {
        Self { repo_dir, state_root }
    }

    /// The on-disk worktree path for a sandbox, derived from the per-sandbox
    /// state layout `<state_root>/sandboxes/<name>/worktree-<name>`.
    fn worktree_path(&self, name: &SandboxName) -> PathBuf {
        self.state_root
            .join("sandboxes")
            .join(name.as_str())
            .join(format!("worktree-{}", name.as_str()))
    }
}

impl WorktreeProvider for GitWorktreeProvider {
    fn create(&self, name: &SandboxName, branch: &BranchName) -> Result<Worktree, HortError> {
        let path = self.worktree_path(name);
        // Clear any stale registration left by a worktree directory that vanished
        // outside hort, so the add does not refuse the path or the branch the dead
        // entry still holds. This is also the resume path for a half-built sandbox.
        run(&self.repo_dir, "worktree prune", &["worktree", "prune"])?;
        let path_arg = path.to_string_lossy();
        if self.branch_exists(branch)? {
            run(
                &self.repo_dir,
                "worktree add",
                &["worktree", "add", path_arg.as_ref(), branch.as_str()],
            )?;
        } else {
            // Pin the source to HEAD so a repository without commits fails here
            // rather than git silently inferring an orphan branch.
            run(
                &self.repo_dir,
                "worktree add",
                &["worktree", "add", "-b", branch.as_str(), path_arg.as_ref(), "HEAD"],
            )?;
        }
        Ok(Worktree { path })
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        let path = self.worktree_path(name);
        let porcelain = run(&self.repo_dir, "worktree list", &["worktree", "list", "--porcelain"])?;
        if !parse_worktree_paths(&porcelain).into_iter().any(|listed| listed == path) {
            return Ok(());
        }
        if path.exists() {
            let path_arg = path.to_string_lossy();
            run(
                &self.repo_dir,
                "worktree remove",
                &["worktree", "remove", "--force", path_arg.as_ref()],
            )?;
        } else {
            run(&self.repo_dir, "worktree prune", &["worktree", "prune"])?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<Worktree>, HortError> {
        let porcelain = run(&self.repo_dir, "worktree list", &["worktree", "list", "--porcelain"])?;
        Ok(parse_worktree_paths(&porcelain)
            .into_iter()
            .filter(|path| path.exists())
            .map(|path| Worktree { path })
            .collect())
    }

    fn is_git_repo(&self) -> Result<bool, HortError> {
        let output =
            capture(&self.repo_dir, "rev-parse", &["rev-parse", "--is-inside-work-tree"])?;
        Ok(output.status.success())
    }

    fn branch_exists(&self, branch: &BranchName) -> Result<bool, HortError> {
        let refname = format!("refs/heads/{}", branch.as_str());
        let output =
            capture(&self.repo_dir, "show-ref", &["show-ref", "--verify", "--quiet", &refname])?;
        Ok(output.status.success())
    }

    fn is_checked_out(&self, branch: &BranchName) -> Result<bool, HortError> {
        let porcelain = run(&self.repo_dir, "worktree list", &["worktree", "list", "--porcelain"])?;
        let checked_out = format!("branch refs/heads/{}", branch.as_str());
        Ok(porcelain.lines().any(|line| line == checked_out))
    }

    fn is_dirty(&self, name: &SandboxName) -> Result<bool, HortError> {
        let porcelain = run(&self.worktree_path(name), "status", &["status", "--porcelain"])?;
        Ok(!porcelain.trim().is_empty())
    }
}

/// Run `git -C <dir> <args>`, mapping only a spawn failure to a domain error.
/// The exit status is left for the caller to interpret, so probes that treat a
/// non-zero exit as a boolean answer (not a failure) can read `status.success()`.
fn capture(dir: &Path, op: &str, args: &[&str]) -> Result<Output, HortError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| HortError::GitCommandFailed { detail: format!("{op}: {err}") })
}

/// Run `git -C <dir> <args>` and require success, returning its stdout. A
/// non-zero exit maps to a domain error whose detail names the operation and
/// carries git's stderr.
fn run(dir: &Path, op: &str, args: &[&str]) -> Result<String, HortError> {
    let output = capture(dir, op, args)?;
    if !output.status.success() {
        return Err(HortError::GitCommandFailed {
            detail: format!("{op}: {}", String::from_utf8_lossy(&output.stderr).trim()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The host paths of every worktree in `git worktree list --porcelain` output,
/// in listed order and unfiltered: each record opens with a `worktree <path>`
/// line, the main checkout included.
fn parse_worktree_paths(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    /// A scratch directory whose path is canonicalized, so the worktree paths the
    /// provider derives and the paths git reports back compare equal regardless of
    /// any symlinks in the temp root.
    fn temp_dir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    /// Run a real git command in `dir`, asserting it succeeded, and return its
    /// stdout. Test arrangements and assertions drive git directly; git is the
    /// external system whose persisted state is the effect under test.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn init_repo_with_commit(repo: &Path) {
        git(repo, &["init", "-b", "main"]);
        fs::write(repo.join("README.md"), "seed\n").unwrap();
        git(repo, &["add", "README.md"]);
        git(
            repo,
            &[
                "-c",
                "user.name=hort-test",
                "-c",
                "user.email=hort-test@localhost",
                "commit",
                "-m",
                "seed",
            ],
        );
    }

    fn init_repo_without_commit(repo: &Path) {
        git(repo, &["init", "-b", "main"]);
    }

    fn commit_in_worktree(worktree: &Path, file: &str, contents: &str) -> String {
        fs::write(worktree.join(file), contents).unwrap();
        git(worktree, &["add", file]);
        git(
            worktree,
            &[
                "-c",
                "user.name=hort-test",
                "-c",
                "user.email=hort-test@localhost",
                "commit",
                "-m",
                "work",
            ],
        );
        rev_parse(worktree, "HEAD")
    }

    fn rev_parse(dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev]).trim().to_string()
    }

    fn current_branch(worktree: &Path) -> String {
        git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string()
    }

    fn canonical_worktree(state_root: &Path, name: &SandboxName) -> PathBuf {
        state_root
            .join("sandboxes")
            .join(name.as_str())
            .join(format!("worktree-{}", name.as_str()))
    }

    #[test]
    fn git_worktree_create_creates_branch_from_head() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let head = rev_parse(&repo, "HEAD");
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let worktree = provider.create(&name, &branch).unwrap();

        assert_eq!(worktree.path, canonical_worktree(&state_root, &name));
        assert_eq!(rev_parse(&repo, "refs/heads/demo"), head);
        assert_eq!(current_branch(&worktree.path), "demo");
    }

    #[test]
    fn git_worktree_create_checks_out_existing_branch() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("work").unwrap();
        let branch = BranchName::new("feature-x").unwrap();

        let worktree = provider.create(&name, &branch).unwrap();

        assert_eq!(worktree.path, canonical_worktree(&state_root, &name));
        assert_eq!(current_branch(&worktree.path), "feature-x");
    }

    #[test]
    fn git_worktree_create_recovers_when_worktree_directory_vanished() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        let first = provider.create(&name, &branch).unwrap();
        fs::remove_dir_all(&first.path).unwrap();

        let again = provider.create(&name, &branch).unwrap();

        assert_eq!(again.path, canonical_worktree(&state_root, &name));
        assert!(again.path.exists());
        assert_eq!(current_branch(&again.path), "demo");
    }

    #[test]
    fn git_worktree_create_propagates_git_failure() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_without_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let result = provider.create(&name, &branch);

        assert!(matches!(result, Err(HortError::GitCommandFailed { .. })));
    }

    #[test]
    fn git_worktree_list_includes_created_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();

        let listed = provider.list().unwrap();

        let canonical = canonical_worktree(&state_root, &name);
        assert!(listed.iter().any(|worktree| worktree.path == canonical));
    }

    #[test]
    fn git_worktree_list_excludes_worktree_whose_directory_vanished() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_dir_all(&worktree.path).unwrap();

        let listed = provider.list().unwrap();

        let canonical = canonical_worktree(&state_root, &name);
        assert!(!listed.iter().any(|entry| entry.path == canonical));
    }

    #[test]
    fn git_worktree_remove_deletes_worktree_directory() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();

        provider.remove(&name).unwrap();

        assert!(!worktree.path.exists());
        let listed = provider.list().unwrap();
        assert!(!listed.iter().any(|entry| entry.path == worktree.path));
    }

    #[test]
    fn git_worktree_remove_is_idempotent_for_missing_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("never-created").unwrap();

        assert!(provider.remove(&name).is_ok());
    }

    #[test]
    fn git_worktree_remove_clears_stale_registration() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_dir_all(&worktree.path).unwrap();

        provider.remove(&name).unwrap();

        let porcelain = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!porcelain.contains("worktree-demo"));
    }

    #[test]
    fn git_worktree_remove_leaves_branch_and_commits_intact() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        let tip = commit_in_worktree(&worktree.path, "work.txt", "work\n");

        provider.remove(&name).unwrap();

        assert_eq!(rev_parse(&repo, "refs/heads/demo"), tip);
    }

    #[test]
    fn git_worktree_is_git_repo_reports_true_inside_repo() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(provider.is_git_repo().unwrap());
    }

    #[test]
    fn git_worktree_is_git_repo_reports_false_outside_repo() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(!provider.is_git_repo().unwrap());
    }

    #[test]
    fn git_worktree_branch_exists_reports_existing_branch() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(provider.branch_exists(&BranchName::new("feature-x").unwrap()).unwrap());
    }

    #[test]
    fn git_worktree_branch_exists_reports_missing_branch() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(!provider.branch_exists(&BranchName::new("absent").unwrap()).unwrap());
    }

    #[test]
    fn git_worktree_is_checked_out_detects_branch_in_main_checkout() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(provider.is_checked_out(&BranchName::new("main").unwrap()).unwrap());
    }

    #[test]
    fn git_worktree_is_checked_out_reports_false_for_unchecked_branch() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        assert!(!provider.is_checked_out(&BranchName::new("feature-x").unwrap()).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_reports_false_for_clean_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();

        assert!(!provider.is_dirty(&name).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_reports_true_for_modified_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::write(worktree.path.join("README.md"), "changed\n").unwrap();

        assert!(provider.is_dirty(&name).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_reports_true_for_untracked_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::write(worktree.path.join("untracked.txt"), "x\n").unwrap();

        assert!(provider.is_dirty(&name).unwrap());
    }
}
