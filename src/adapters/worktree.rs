//! `GitWorktreeProvider`: the git-backed `WorktreeProvider`. Shells the system
//! `git` (never a git library) to add, list, remove, and inspect the worktrees
//! that back each sandbox's `/workdir`, one per sandbox at
//! `<state_root>/sandboxes/<name>/worktree-<name>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::domain::config::GitMode;
use crate::domain::error::HortError;
use crate::domain::model::{BranchName, SandboxName};
use crate::domain::mounts::GIT_OBJECTS;
use crate::ports::{Worktree, WorktreeProvider};

/// The name a clone knows the host repository by. It exists so a box can bring
/// host work in, never to send work out: the remote a `git clone --shared`
/// leaves behind accepts a push straight into the host repository, which is the
/// write the sandbox model forbids.
const HOST_REMOTE: &str = "hort-base";

/// The push address of `HOST_REMOTE`, which names no repository anywhere. That
/// is what closes the write channel: measured on git 2.55, an empty push
/// address is ignored and the push goes through, while this one git refuses by
/// itself, naming the reason in the message the agent reads.
const HOST_REMOTE_PUSH_URL: &str = "hort-base-is-fetch-only";

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

    /// Add a worktree of the project repository at `path`, checked out on
    /// `branch`.
    fn add_worktree(&self, path: &Path, branch: &BranchName) -> Result<(), HortError> {
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
        Ok(())
    }

    /// Give the sandbox a repository of its own at `path`, checked out on
    /// `branch`. One act rather than several, because each step past the clone
    /// itself is what keeps the clone usable: without the pin the host's own gc
    /// can prune an object the clone borrows, without the rewired remotes the
    /// box can write the host repository, and without the recorded object path
    /// the clone finds no objects inside the box.
    fn clone_repository(
        &self,
        name: &SandboxName,
        path: &Path,
        branch: &BranchName,
    ) -> Result<(), HortError> {
        let source_arg = self.repo_dir.to_string_lossy();
        let path_arg = path.to_string_lossy();
        run(
            &self.repo_dir,
            "clone",
            &["clone", "--shared", source_arg.as_ref(), path_arg.as_ref()],
        )?;
        if self.branch_exists(branch)? {
            run(path, "checkout", &["checkout", branch.as_str()])?;
        } else {
            run(path, "checkout", &["checkout", "-b", branch.as_str()])?;
        }
        self.wire_remotes(path)?;
        self.pin_clone_base(name, path)?;
        record_box_object_store(path)
    }

    /// Point the clone's remotes where a push is safe. `origin` becomes the host
    /// repository's own remote, so an agent pushes its branch where the project
    /// lives and opens a pull request with nothing to configure; a project with
    /// no remote of its own leaves the clone without one, because hort invents a
    /// remote no more than it invents an allowlist. The host repository stays
    /// reachable for fetching, under a name whose push address resolves nowhere.
    fn wire_remotes(&self, clone: &Path) -> Result<(), HortError> {
        match self.host_remote_url()? {
            Some(url) => {
                run(clone, "remote set-url", &["remote", "set-url", "origin", &url])?;
            }
            None => {
                run(clone, "remote remove", &["remote", "remove", "origin"])?;
            }
        }
        let source_arg = self.repo_dir.to_string_lossy();
        run(clone, "remote add", &["remote", "add", HOST_REMOTE, source_arg.as_ref()])?;
        run(
            clone,
            "remote set-url",
            &["remote", "set-url", "--push", HOST_REMOTE, HOST_REMOTE_PUSH_URL],
        )?;
        Ok(())
    }

    /// Where the project repository itself pushes, and nothing when it pushes
    /// nowhere. Absence is an answer here and not a failure, so a repository
    /// with no remote is read rather than refused.
    fn host_remote_url(&self) -> Result<Option<String>, HortError> {
        let output = capture(&self.repo_dir, "remote get-url", &["remote", "get-url", "origin"])?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }

    /// Hold the commit the clone stands on in the host repository, so the host's
    /// own gc cannot prune an object the clone borrows instead of owning. A
    /// repository with no commit stands on nothing and has no object a gc could
    /// prune, so it is left unpinned rather than refused, the way the worktree
    /// route accepts one too.
    fn pin_clone_base(&self, name: &SandboxName, clone: &Path) -> Result<(), HortError> {
        let base = capture(clone, "rev-parse", &["rev-parse", "HEAD"])?;
        if !base.status.success() {
            return Ok(());
        }
        let commit = String::from_utf8_lossy(&base.stdout).trim().to_string();
        run(&self.repo_dir, "update-ref", &["update-ref", &self.base_pin(name), &commit])
            .map(|_| ())
    }

    /// Release the commit this sandbox stood on, so the host's own gc is free to
    /// collect it again: once the clone is gone the pin holds an object for
    /// nobody. It is named after the one sandbox rather than swept, because the
    /// other pins are what keep the boxes still standing one `git gc` away from
    /// a `/workdir` whose objects cannot be read.
    ///
    /// Only clone mode ever writes one, and deleting a ref that is not there is
    /// already the state asked for, so the worktree route passes through
    /// untouched and a clone whose directory vanished still gets collected.
    fn unpin_clone_base(&self, name: &SandboxName) -> Result<(), HortError> {
        run(&self.repo_dir, "update-ref", &["update-ref", "-d", &self.base_pin(name)]).map(|_| ())
    }

    /// The ref the host repository holds a sandbox's base commit under.
    fn base_pin(&self, name: &SandboxName) -> String {
        format!("refs/hort/{}/base", name.as_str())
    }
}

impl WorktreeProvider for GitWorktreeProvider {
    fn create(
        &self,
        name: &SandboxName,
        branch: &BranchName,
        mode: GitMode,
    ) -> Result<Worktree, HortError> {
        let path = self.worktree_path(name);
        match mode {
            GitMode::Worktree => self.add_worktree(&path, branch)?,
            GitMode::Clone => self.clone_repository(name, &path, branch)?,
        }
        Ok(Worktree { path })
    }

    fn remove(&self, name: &SandboxName) -> Result<(), HortError> {
        self.unpin_clone_base(name)?;
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

    fn git_mode_at(&self, path: &Path) -> Option<GitMode> {
        // The shape of `.git` is the whole answer, and no git command is needed
        // for it: a clone carries its repository in a directory of its own,
        // while a worktree carries a file pointing at the administrative
        // directory the project repository keeps for it.
        let git = path.join(".git");
        if git.is_dir() {
            Some(GitMode::Clone)
        } else if git.is_file() {
            Some(GitMode::Worktree)
        } else {
            None
        }
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

    fn holds_unreturned_work(&self, name: &SandboxName, project: &Path) -> Result<bool, HortError> {
        // Two host-side reads, neither of which enters the box. Resolving the
        // tip touches no object, so it answers even inside a clone whose
        // borrowed objects are mounted only in the sandbox. A worktree's tip is
        // always a commit the project repository has, since that is the object
        // store it committed into, so this reads false there without a mode of
        // its own. `cat-file -e` exits 0 for a commit the project has and 1 for
        // one it lacks; anything else, such as 128 in a folder that is no
        // repository, is a question the project could not answer.
        let workdir = self.worktree_path(name);
        let tip = run(&workdir, "rev-parse", &["rev-parse", "HEAD"])?;
        let lookup = capture(project, "cat-file", &["cat-file", "-e", tip.trim()])?;
        match lookup.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(HortError::GitCommandFailed {
                detail: format!("cat-file: {}", String::from_utf8_lossy(&lookup.stderr).trim()),
            }),
        }
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

/// Record inside the clone where the objects it borrows will be, which is a
/// path of the box and not of the host: git resolves that path on whichever
/// machine it runs on, and this clone is read inside the box. Written last,
/// because from here on the host itself can no longer read the clone's objects.
fn record_box_object_store(clone: &Path) -> Result<(), HortError> {
    let alternates = clone.join(".git").join("objects").join("info").join("alternates");
    std::fs::write(&alternates, format!("{GIT_OBJECTS}\n"))
        .map_err(|err| HortError::GitCommandFailed { detail: format!("clone alternates: {err}") })
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

    use crate::domain::mounts::GIT_OBJECTS;

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

    /// Run a real git command against a clone, lending it the host object store
    /// through the environment, and return the raw outcome. A clone records the
    /// object path the box will have and the host does not, so a test that
    /// drives git against one has to supply the borrow the box gets by mount:
    /// without it every object read fails and a push is refused for a reason
    /// that has nothing to do with the remote under test.
    fn git_borrowing_objects(dir: &Path, objects: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .current_dir(dir)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", objects)
            .args(args)
            .output()
            .unwrap()
    }

    /// Commit inside a clone the way the box does, lending it the host object
    /// store the box has by mount, and return the commit it left at the tip.
    fn commit_in_clone(clone: &Path, objects: &Path, file: &str) -> String {
        fs::write(clone.join(file), "work\n").unwrap();
        let staged = git_borrowing_objects(clone, objects, &["add", file]);
        assert!(staged.status.success(), "git add failed in the clone");
        let committed = git_borrowing_objects(
            clone,
            objects,
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
        assert!(committed.status.success(), "git commit failed in the clone");
        let head = git_borrowing_objects(clone, objects, &["rev-parse", "HEAD"]);
        assert!(head.status.success(), "git rev-parse failed in the clone");
        String::from_utf8(head.stdout).unwrap().trim().to_string()
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

        let worktree = provider.create(&name, &branch, GitMode::Worktree).unwrap();

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

        let worktree = provider.create(&name, &branch, GitMode::Worktree).unwrap();

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
        let first = provider.create(&name, &branch, GitMode::Worktree).unwrap();
        fs::remove_dir_all(&first.path).unwrap();

        let again = provider.create(&name, &branch, GitMode::Worktree).unwrap();

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

        let result = provider.create(&name, &branch, GitMode::Worktree);

        assert!(matches!(result, Err(HortError::GitCommandFailed { .. })));
    }

    #[test]
    fn git_worktree_list_includes_created_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        provider.create(&name, &BranchName::new("feature-x").unwrap(), GitMode::Worktree).unwrap();

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
            .create(
                &SandboxName::new("work").unwrap(),
                &BranchName::new("feature-x").unwrap(),
                GitMode::Worktree,
            )
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
            .create(
                &SandboxName::new("work").unwrap(),
                &BranchName::new("feature-x").unwrap(),
                GitMode::Worktree,
            )
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
        provider.create(&name, &BranchName::new("feature-x").unwrap(), GitMode::Worktree).unwrap();

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
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

        assert!(!provider.is_dirty(&name).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_reports_true_for_modified_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

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
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
        fs::remove_dir_all(&worktree.path).unwrap();

        provider.prune_stale().unwrap();

        let porcelain = git(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!porcelain.contains("worktree-demo"));
    }

    #[test]
    fn clone_mode_create_gives_the_sandbox_its_own_git_directory() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(workdir.path, canonical_worktree(&state_root, &name));
        assert!(workdir.path.join(".git").is_dir());
    }

    #[test]
    fn clone_mode_create_checks_out_a_new_branch_named_after_the_sandbox() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(current_branch(&workdir.path), "demo");
    }

    #[test]
    fn clone_mode_create_checks_out_an_existing_branch() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["branch", "feature-x"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("work").unwrap();
        let branch = BranchName::new("feature-x").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(current_branch(&workdir.path), "feature-x");
    }

    #[test]
    fn clone_mode_create_pins_the_clone_base_commit_in_the_host_repository() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let base = rev_parse(&repo, "HEAD");
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(rev_parse(&repo, "refs/hort/demo/base"), base);
    }

    #[test]
    fn clone_mode_create_points_the_clone_at_the_object_store_the_box_will_have() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        let alternates =
            fs::read_to_string(workdir.path.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(alternates.trim(), GIT_OBJECTS);
    }

    #[test]
    fn clone_mode_create_gives_the_clone_the_host_repositorys_own_origin() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        git(&repo, &["remote", "add", "origin", "https://github.com/example/thing.git"]);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(
            git(&workdir.path, &["remote", "get-url", "origin"]).trim(),
            "https://github.com/example/thing.git"
        );
    }

    #[test]
    fn clone_mode_create_leaves_no_origin_when_the_host_repository_has_none() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        let remotes = git(&workdir.path, &["remote"]);
        assert!(!remotes.lines().any(|remote| remote.trim() == "origin"), "remotes: {remotes}");
    }

    #[test]
    fn clone_mode_create_names_the_host_repository_as_the_hort_base_remote() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();

        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        assert_eq!(
            Path::new(git(&workdir.path, &["remote", "get-url", "hort-base"]).trim()),
            repo.as_path()
        );
    }

    #[test]
    fn a_push_to_the_hort_base_remote_does_not_write_the_host_repository() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();

        git_borrowing_objects(
            &workdir.path,
            &repo.join(".git").join("objects"),
            &["push", "hort-base", "demo"],
        );

        assert_eq!(git(&repo, &["branch", "--list", "demo"]).trim(), "");
    }

    #[test]
    fn clone_mode_reports_work_the_host_repository_does_not_have() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        let workdir = provider.create(&name, &branch, GitMode::Clone).unwrap();
        commit_in_clone(&workdir.path, &repo.join(".git").join("objects"), "work.txt");

        assert!(provider.holds_unreturned_work(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_reports_no_unreturned_work_when_the_host_repository_has_the_tip() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        provider.create(&name, &branch, GitMode::Clone).unwrap();

        // A fresh clone stands on the commit it was cloned from, which is the
        // host's own. The pair matters more than either half: without this one a
        // probe that always says yes passes, and a guard that always asks turns
        // every `down` of a clone into a prompt the user learns to answer blind.
        assert!(!provider.holds_unreturned_work(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_asks_the_project_it_names_rather_than_the_repository_it_runs_from() {
        let (_project, project) = temp_dir();
        let (_elsewhere, elsewhere) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&project);
        git(&elsewhere, &["init", "-b", "main"]);
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        GitWorktreeProvider::new(project.clone(), state_root.clone())
            .create(&name, &branch, GitMode::Clone)
            .unwrap();
        let run_from_elsewhere = GitWorktreeProvider::new(elsewhere, state_root);

        // `ls`, `down` and `prune` are global, so the repository a provider is
        // rooted at is whichever one the user is standing in. This clone holds
        // nothing its own project lacks, and asked of any other repository it
        // would read as holding work: every box of another project would be
        // flagged, and every `down` of one would ask about commits that exist.
        assert!(!run_from_elsewhere.holds_unreturned_work(&name, &project).unwrap());
    }

    #[test]
    fn clone_mode_cannot_answer_for_a_project_that_is_not_a_git_repository() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        let (_plain, plain) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo, state_root);
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        provider.create(&name, &branch, GitMode::Clone).unwrap();

        // A project folder that was deleted, or stopped being a repository,
        // says nothing about where the clone's commits are. Read as "holding
        // work" it prints a claim nobody checked; read as "nothing held" it lets
        // a teardown take commits it never looked for.
        assert!(provider.holds_unreturned_work(&name, &plain).is_err());
    }

    #[test]
    fn clone_mode_remove_deletes_the_pinned_base_ref_from_the_host_repository() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let branch = BranchName::new("demo").unwrap();
        provider.create(&name, &branch, GitMode::Clone).unwrap();

        provider.remove(&name).unwrap();

        // The pin exists to keep the host's gc off an object the clone borrows.
        // Once the clone is gone it holds a commit for nobody, in a repository
        // no hort command will ever visit again with that sandbox's name.
        let pinned = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "--verify", "--quiet", "refs/hort/demo/base"])
            .output()
            .unwrap();
        assert!(!pinned.status.success(), "the pinned base ref outlived the sandbox");
    }

    #[test]
    fn clone_mode_remove_leaves_another_sandboxs_pinned_base_ref_intact() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let base = rev_parse(&repo, "HEAD");
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let going = SandboxName::new("demo").unwrap();
        let staying = SandboxName::new("other").unwrap();
        provider.create(&going, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        provider.create(&staying, &BranchName::new("other").unwrap(), GitMode::Clone).unwrap();

        provider.remove(&going).unwrap();

        // The pin is what keeps the host's gc off the objects a clone borrows
        // rather than owns, so collecting one box by sweeping every pin leaves
        // the boxes still standing one `git gc` away from an unreadable
        // `/workdir`.
        assert_eq!(rev_parse(&repo, "refs/hort/other/base"), base);
    }

    #[test]
    fn git_mode_at_reports_clone_for_a_workdir_holding_a_clone() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let workdir = state_root.join("workdir");
        git(&repo, &["clone", "--shared", repo.to_str().unwrap(), workdir.to_str().unwrap()]);
        let provider = GitWorktreeProvider::new(repo, state_root);

        assert_eq!(provider.git_mode_at(&workdir), Some(GitMode::Clone));
    }

    #[test]
    fn git_mode_at_reports_worktree_for_a_workdir_holding_a_worktree() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let workdir = state_root.join("workdir");
        git(&repo, &["worktree", "add", "-b", "demo", workdir.to_str().unwrap(), "HEAD"]);
        let provider = GitWorktreeProvider::new(repo, state_root);

        assert_eq!(provider.git_mode_at(&workdir), Some(GitMode::Worktree));
    }

    #[test]
    fn git_mode_at_reports_nothing_for_a_directory_holding_no_git() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let workdir = state_root.join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let provider = GitWorktreeProvider::new(repo, state_root);

        assert_eq!(provider.git_mode_at(&workdir), None);
    }
}
