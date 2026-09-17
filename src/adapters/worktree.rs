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

    /// The administrative directory git keeps on the host for the linked
    /// worktree at `worktree`. Discovered by matching git's own `gitdir` records
    /// under `<repo>/.git/worktrees/`, never by reading the worktree's `.git`
    /// pointer: that file lives on the writable `/workdir` a tenant controls, so
    /// inspecting the worktree through it would run whatever git command the
    /// tenant configured there. Absence of a match is reported as a git failure,
    /// so a worktree hort cannot vouch for degrades to an error rather than a
    /// false verdict.
    fn worktree_admin_dir(&self, worktree: &Path) -> Result<PathBuf, HortError> {
        let missing = || HortError::GitCommandFailed {
            detail: format!("worktree admin dir: none registered for {}", worktree.display()),
        };
        let dot_git = std::fs::canonicalize(worktree.join(".git")).map_err(|_| missing())?;
        let registry = self.repo_dir.join(".git").join("worktrees");
        let entries = std::fs::read_dir(&registry).map_err(|err| HortError::GitCommandFailed {
            detail: format!("worktree admin dir: {err}"),
        })?;
        entries
            .flatten()
            .map(|entry| entry.path())
            .find(|admin| {
                std::fs::read_to_string(admin.join("gitdir"))
                    .ok()
                    .and_then(|recorded| std::fs::canonicalize(recorded.trim()).ok())
                    .is_some_and(|resolved| resolved == dot_git)
            })
            .ok_or_else(missing)
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
        let records = parse_worktree_records(&porcelain);
        let Some(record) = records.iter().find(|record| record.path == path) else {
            return Ok(());
        };
        // git refuses to remove a working tree whose `.git` pointer was deleted
        // under it, so sweeping the registration is the only route that collects
        // such an entry. Which state it is in is git's own verdict, already
        // carried by the listing read above, rather than a rule re-derived here
        // from the pointer file.
        if path.exists() && !record.is_prunable() {
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
        Ok(parse_worktree_records(&porcelain)
            .into_iter()
            .map(|record| record.path)
            .filter(|path| path.exists())
            .map(|path| Worktree { path })
            .collect())
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_git_repo(&self) -> Result<bool, HortError> {
        let output = capture(&self.repo_dir, "rev-parse", &["rev-parse", "--is-inside-work-tree"])?;
        Ok(output.status.success())
    }

    fn branch_exists(&self, branch: &BranchName) -> Result<bool, HortError> {
        let refname = format!("refs/heads/{}", branch.as_str());
        let output =
            capture(&self.repo_dir, "show-ref", &["show-ref", "--verify", "--quiet", &refname])?;
        Ok(output.status.success())
    }

    fn checked_out_at(&self, branch: &BranchName) -> Result<Vec<PathBuf>, HortError> {
        let porcelain = run(&self.repo_dir, "worktree list", &["worktree", "list", "--porcelain"])?;
        Ok(parse_worktree_records(&porcelain)
            .into_iter()
            .filter(|record| record.holds(branch))
            .map(|record| record.path)
            .collect())
    }

    fn branch_held_at(&self, path: &Path) -> Result<Option<BranchName>, HortError> {
        let porcelain = run(&self.repo_dir, "worktree list", &["worktree", "list", "--porcelain"])?;
        parse_worktree_records(&porcelain)
            .iter()
            .find(|record| record.path == path)
            .and_then(WorktreeRecord::held_branch)
            .map(BranchName::new)
            .transpose()
    }

    fn is_dirty(&self, name: &SandboxName) -> Result<bool, HortError> {
        let worktree = self.worktree_path(name);
        let admin = self.worktree_admin_dir(&worktree)?;
        let admin_arg = admin.to_string_lossy();
        let worktree_arg = worktree.to_string_lossy();
        let porcelain = run(
            &self.repo_dir,
            "status",
            &[
                "--git-dir",
                admin_arg.as_ref(),
                "--work-tree",
                worktree_arg.as_ref(),
                "status",
                "--porcelain",
            ],
        )?;
        Ok(!porcelain.trim().is_empty())
    }

    fn prune_stale(&self) -> Result<(), HortError> {
        run(&self.repo_dir, "worktree prune", &["worktree", "prune"]).map(|_| ())
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

/// One worktree as `git worktree list --porcelain` reports it: the host path on
/// the `worktree <path>` line that opens the record, and the attribute lines
/// that follow it up to the next record.
struct WorktreeRecord<'a> {
    path: PathBuf,
    attributes: Vec<&'a str>,
}

impl WorktreeRecord<'_> {
    /// Whether git marks this worktree prunable, its own name for an entry
    /// `git worktree prune` collects. The marker may carry a reason.
    fn is_prunable(&self) -> bool {
        self.attributes.iter().any(|line| *line == "prunable" || line.starts_with("prunable "))
    }

    /// Whether this worktree has `branch` checked out. The whole line is matched,
    /// because a branch whose name only starts with this one is a different branch.
    fn holds(&self, branch: &BranchName) -> bool {
        let checked_out = format!("branch refs/heads/{}", branch.as_str());
        self.attributes.iter().any(|line| *line == checked_out)
    }

    /// The short name of the branch this worktree has checked out, absent for a
    /// detached HEAD, which git reports with a `detached` line instead.
    fn held_branch(&self) -> Option<&str> {
        self.attributes.iter().find_map(|line| line.strip_prefix("branch refs/heads/"))
    }
}

/// Every worktree record in `git worktree list --porcelain` output, in listed
/// order and unfiltered, the main checkout included.
fn parse_worktree_records(porcelain: &str) -> Vec<WorktreeRecord<'_>> {
    let mut records: Vec<WorktreeRecord<'_>> = Vec::new();
    for line in porcelain.lines() {
        if let Some(listed) = line.strip_prefix("worktree ") {
            records.push(WorktreeRecord { path: PathBuf::from(listed), attributes: Vec::new() });
        } else if let Some(record) = records.last_mut() {
            record.attributes.push(line);
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
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
        state_root.join("sandboxes").join(name.as_str()).join(format!("worktree-{}", name.as_str()))
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
    fn git_worktree_exists_reports_a_directory_this_repository_does_not_list() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let elsewhere = canonical_worktree(&state_root, &SandboxName::new("other").unwrap());
        fs::create_dir_all(&elsewhere).unwrap();

        // A worktree belonging to another project is on disk and absent from
        // this repository's list. Answering from the list is what would make a
        // running sandbox read as one whose worktree vanished, from any
        // directory but its own.
        assert!(provider.exists(&elsewhere));
    }

    #[test]
    fn git_worktree_exists_reports_false_for_a_directory_that_is_gone() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_dir_all(&worktree.path).unwrap();

        assert!(!provider.exists(&worktree.path));
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
    fn git_worktree_remove_succeeds_when_the_git_pointer_vanished() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_file(worktree.path.join(".git")).unwrap();

        // An agent that deletes the pointer file of its own worktree, which a
        // distracted recursive remove produces, leaves a directory git refuses
        // to remove. Without a route out of that state the sandbox the user
        // named can only be collected by hand.
        assert!(provider.remove(&name).is_ok());
    }

    #[test]
    fn git_worktree_remove_clears_registration_when_the_git_pointer_vanished() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_file(worktree.path.join(".git")).unwrap();

        provider.remove(&name).unwrap();

        let porcelain = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!porcelain.contains("worktree-demo"));
    }

    #[test]
    fn git_worktree_remove_leaves_the_branch_intact_when_the_git_pointer_vanished() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        let tip = commit_in_worktree(&worktree.path, "work.txt", "work\n");
        fs::remove_file(worktree.path.join(".git")).unwrap();

        provider.remove(&name).unwrap();

        // Committed work survives the box that produced it, and the route that
        // collects a worktree with no pointer is a second way into that
        // promise. The branch is the only copy left once the worktree is gone.
        assert_eq!(rev_parse(&repo, "refs/heads/demo"), tip);
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
    fn git_worktree_reports_a_branch_checked_out_in_a_sandbox_worktree_at_that_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("work").unwrap();
        provider.create(&name, &BranchName::new("feature-x").unwrap()).unwrap();

        let holders = provider.checked_out_at(&BranchName::new("feature-x").unwrap()).unwrap();

        // Spelled the way the sandbox's own worktree path is derived, because a
        // caller tells its own worktree from another one by comparing the two.
        assert_eq!(holders, vec![canonical_worktree(&state_root, &name)]);
    }

    #[test]
    fn git_worktree_reports_a_branch_checked_out_in_the_main_checkout_at_the_repository() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        provider
            .create(&SandboxName::new("work").unwrap(), &BranchName::new("feature-x").unwrap())
            .unwrap();

        let holders = provider.checked_out_at(&BranchName::new("main").unwrap()).unwrap();

        assert_eq!(holders, vec![repo]);
    }

    #[test]
    fn git_worktree_reports_no_worktree_for_a_branch_no_worktree_holds() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature"]);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        // A branch whose name the held one starts with, so that answering by
        // the start of a name reads a free branch as taken.
        provider
            .create(&SandboxName::new("work").unwrap(), &BranchName::new("feature-x").unwrap())
            .unwrap();

        let holders = provider.checked_out_at(&BranchName::new("feature").unwrap()).unwrap();

        assert_eq!(holders, Vec::<PathBuf>::new());
    }

    #[test]
    fn git_worktree_reports_the_branch_a_sandbox_worktree_holds() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("work").unwrap();
        provider.create(&name, &BranchName::new("feature-x").unwrap()).unwrap();

        let held = provider.branch_held_at(&canonical_worktree(&state_root, &name)).unwrap();

        // The main checkout is listed first and holds another branch, so an
        // answer that is not read off this worktree's own record names `main`.
        assert_eq!(held, Some(BranchName::new("feature-x").unwrap()));
    }

    #[test]
    fn git_worktree_reports_no_branch_at_a_path_no_worktree_is_at() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());

        let held = provider
            .branch_held_at(&canonical_worktree(&state_root, &SandboxName::new("work").unwrap()))
            .unwrap();

        assert_eq!(held, None);
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

    #[test]
    fn git_worktree_is_dirty_fails_when_the_parent_repository_is_gone() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_dir_all(&repo).unwrap();

        let result = provider.is_dirty(&name);

        // The worktree is still on disk and is now the only copy of everything
        // it holds, committed work included. Answering "clean" here would let
        // the guard that protects uncommitted work delete the last copy.
        assert!(matches!(result, Err(HortError::GitCommandFailed { .. })));
    }

    #[test]
    fn git_worktree_is_dirty_does_not_run_a_command_the_worktree_pointer_names() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();

        // A tenant with write access to /workdir rewrites the worktree's `.git`
        // pointer to name a repository it planted inside the worktree, whose
        // config runs a command of its own on any status read. The marker file
        // is that command's only effect, so its presence is the code having run.
        let marker = state_root.join("agent-code-ran");
        let planted = worktree.path.join(".planted-git");
        git(&worktree.path, &["--git-dir", planted.to_str().unwrap(), "init", "--bare"]);
        let hook = planted.join("run-on-status.sh");
        fs::write(&hook, format!("#!/bin/sh\ntouch {}\nprintf '%s\\0' 1\n", marker.display()))
            .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let planted_arg = planted.to_str().unwrap();
        git(
            &worktree.path,
            &["--git-dir", planted_arg, "config", "core.fsmonitor", hook.to_str().unwrap()],
        );
        git(&worktree.path, &["--git-dir", planted_arg, "config", "core.bare", "false"]);
        git(
            &worktree.path,
            &["--git-dir", planted_arg, "config", "core.worktree", worktree.path.to_str().unwrap()],
        );
        fs::write(worktree.path.join(".git"), format!("gitdir: {}\n", planted.display())).unwrap();

        let _ = provider.is_dirty(&name);

        // Inspecting the worktree must read git configuration from the trusted
        // host-side administrative directory, never from the pointer the box
        // controls, so the planted command never runs.
        assert!(!marker.exists());
    }

    #[test]
    fn git_worktree_is_dirty_reads_its_verdict_from_the_admin_dir_not_the_rewritten_pointer() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();

        // The worktree is clean, but its `.git` pointer is rewritten to a fresh
        // empty repository outside it, under which the tracked README reads as
        // untracked and the worktree would look dirty. The pointer is the box's
        // to control, so the verdict has to be read from the trusted host-side
        // administrative directory, where the worktree is clean.
        let decoy = state_root.join("decoy-git");
        let decoy_arg = decoy.to_str().unwrap();
        git(&worktree.path, &["--git-dir", decoy_arg, "init", "--bare"]);
        git(&worktree.path, &["--git-dir", decoy_arg, "config", "core.bare", "false"]);
        git(
            &worktree.path,
            &["--git-dir", decoy_arg, "config", "core.worktree", worktree.path.to_str().unwrap()],
        );
        fs::write(worktree.path.join(".git"), format!("gitdir: {}\n", decoy.display())).unwrap();

        assert!(!provider.is_dirty(&name).unwrap());
    }

    #[test]
    fn git_worktree_prune_stale_clears_vanished_registration() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree = provider.create(&name, &BranchName::new("demo").unwrap()).unwrap();
        fs::remove_dir_all(&worktree.path).unwrap();

        provider.prune_stale().unwrap();

        let porcelain = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!porcelain.contains("worktree-demo"));
    }
}
