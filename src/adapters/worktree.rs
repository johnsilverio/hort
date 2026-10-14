//! `GitWorktreeProvider`: the git-backed `WorktreeProvider`. Shells the system
//! `git` (never a git library) to add, list, remove, and inspect the worktrees
//! that back each sandbox's `/workdir`, one per sandbox at
//! `<state_root>/sandboxes/<name>/worktree-<name>`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// How long hort waits on one read of a `/workdir` before giving up on it,
/// shared by every git that read runs. The sandbox writes that directory, so it
/// can put a named pipe where git opens a file and hold the read for as long as
/// it likes, and with it every command that reads the sandboxes one after
/// another. A read that answers takes milliseconds, and a read given up on is
/// an unknown the callers already treat as protecting the sandbox, never as
/// clearing it.
const WORKDIR_READ_BUDGET: Duration = Duration::from_secs(2);

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

    /// The directory holding one sandbox's state, `<state_root>/sandboxes/<name>`.
    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.state_root.join("sandboxes").join(name.as_str())
    }

    /// The on-disk worktree path for a sandbox, derived from the per-sandbox
    /// state layout `<state_root>/sandboxes/<name>/worktree-<name>`.
    fn worktree_path(&self, name: &SandboxName) -> PathBuf {
        self.sandbox_dir(name).join(format!("worktree-{}", name.as_str()))
    }

    /// The administrative directory the repository at `project` keeps on the
    /// host for the linked worktree at `worktree`. Discovered by matching git's
    /// own `gitdir` records under `<project>/.git/worktrees/`, never by reading the worktree's `.git`
    /// pointer: that file lives on the writable `/workdir` a tenant controls, so
    /// inspecting the worktree through it would run whatever git command the
    /// tenant configured there. Absence of a match is reported as a git failure,
    /// so a worktree hort cannot vouch for degrades to an error rather than a
    /// false verdict.
    fn worktree_admin_dir(project: &Path, worktree: &Path) -> Result<PathBuf, HortError> {
        let missing = || HortError::GitCommandFailed {
            detail: format!("worktree admin dir: none registered for {}", worktree.display()),
        };
        let dot_git = std::fs::canonicalize(worktree.join(".git")).map_err(|_| missing())?;
        let registry = project.join(".git").join("worktrees");
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

    /// Release, in `repository`, the commit this sandbox stood on, so the host's own gc is free to
    /// collect it again: once the clone is gone the pin holds an object for
    /// nobody. It is named after the one sandbox rather than swept, because the
    /// other pins are what keep the boxes still standing one `git gc` away from
    /// a `/workdir` whose objects cannot be read.
    ///
    /// Only clone mode ever writes one, and deleting a ref that is not there is
    /// already the state asked for, so the worktree route passes through
    /// untouched and a clone whose directory vanished still gets collected.
    fn unpin_clone_base(&self, name: &SandboxName, repository: &Path) -> Result<(), HortError> {
        run(repository, "update-ref", &["update-ref", "-d", &self.base_pin(name)]).map(|_| ())
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

    fn remove(&self, name: &SandboxName, project: Option<&Path>) -> Result<(), HortError> {
        self.unpin_clone_base(name, project.unwrap_or(&self.repo_dir))?;
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

    fn is_dirty(&self, name: &SandboxName, project: &Path) -> Result<bool, HortError> {
        let deadline = Instant::now() + WORKDIR_READ_BUDGET;
        let worktree = self.worktree_path(name);
        if self.git_mode_at(&worktree) == Some(GitMode::Clone) {
            return clone_is_dirty(&worktree, &self.sandbox_dir(name), project, deadline);
        }
        let admin = Self::worktree_admin_dir(project, &worktree)?;
        worktree_is_dirty(&worktree, &admin, project, deadline)
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
        let deadline = Instant::now() + WORKDIR_READ_BUDGET;
        let workdir = self.worktree_path(name);
        let tip = run_by(&workdir, "rev-parse", &["rev-parse", "HEAD"], deadline)?;
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

/// `git -C <dir> <args>`, not yet run.
fn git_in(dir: &Path, args: &[&str]) -> Command {
    let mut git = Command::new("git");
    git.arg("-C").arg(dir).args(args);
    git
}

/// Run `git -C <dir> <args>`, mapping only a spawn failure to a domain error.
/// The exit status is left for the caller to interpret, so probes that treat a
/// non-zero exit as a boolean answer (not a failure) can read `status.success()`.
fn capture(dir: &Path, op: &str, args: &[&str]) -> Result<Output, HortError> {
    git_in(dir, args).output().map_err(|err| failed(op, &err))
}

/// Run `git -C <dir> <args>` and require success, returning its stdout.
fn run(dir: &Path, op: &str, args: &[&str]) -> Result<String, HortError> {
    stdout_of_success(op, capture(dir, op, args)?)
}

/// `run` for a git that reads a directory the sandbox writes, given up on at
/// `deadline`.
fn run_by(dir: &Path, op: &str, args: &[&str], deadline: Instant) -> Result<String, HortError> {
    stdout_of_success(op, output_by(&mut git_in(dir, args), op, deadline)?)
}

/// The stdout of a git that exited successfully. A non-zero exit maps to a
/// domain error whose detail names the operation and carries git's stderr.
fn stdout_of_success(op: &str, output: Output) -> Result<String, HortError> {
    if !output.status.success() {
        return Err(failed(op, String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn failed(op: &str, detail: impl std::fmt::Display) -> HortError {
    HortError::GitCommandFailed { detail: format!("{op}: {detail}") }
}

/// Run `git` to completion unless `deadline` comes first, in which case it is
/// killed and waited for, so that no git hort started is left waiting on the
/// sandbox once hort has moved on, and the read fails like any other.
///
/// Both streams are drained while git runs rather than after it ends: a git
/// writing more than a pipe holds stops until somebody reads, so a read that
/// only waited for the end would give up on every large answer.
fn output_by(git: &mut Command, op: &str, deadline: Instant) -> Result<Output, HortError> {
    let mut child = git
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| failed(op, &err))?;
    let (closed, stream_closed) = mpsc::channel();
    let streams = [drain(child.stdout.take(), closed.clone()), drain(child.stderr.take(), closed)];
    let all_closed = streams.iter().all(|_| {
        stream_closed.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok()
    });
    let exited = if all_closed { exited_by(&mut child, deadline) } else { None };
    let Some(status) = exited else {
        // Killed rather than only abandoned: a git waiting on a pipe the
        // sandbox never answers outlives hort otherwise, for good.
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed(op, "no answer from the sandbox's /workdir in time"));
    };
    let [stdout, stderr] = streams.map(|stream| match stream.join() {
        Ok(read) => read.map_err(|err| failed(op, &err)),
        Err(panic) => std::panic::resume_unwind(panic),
    });
    Ok(Output { status, stdout: stdout?, stderr: stderr? })
}

/// Read `stream` to its end on a thread of its own, saying on `closed` when it
/// is done, whether it got there or failed on the way.
fn drain(
    stream: Option<impl Read + Send + 'static>,
    closed: mpsc::Sender<()>,
) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = match stream {
            Some(mut stream) => stream.read_to_end(&mut bytes).map(|_| bytes),
            None => Ok(bytes),
        };
        let _ = closed.send(());
        read
    })
}

/// How `child` exited, or nothing if it has not by `deadline`. Asked once its
/// streams are closed, which a process does as it exits, so the wait is
/// normally over at the first look.
fn exited_by(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// Whether the `/workdir` that `git` reads holds uncommitted changes, read
/// without the host ever running git inside a submodule of it. `git` runs one
/// git command against that `/workdir`, under the name of the operation it is
/// for, and returns what it printed.
///
/// A gitlink in the index cannot be answered either way. Status runs git inside
/// a submodule it finds checked out, under the configuration that checkout
/// carries, which is the sandbox's to write; and ignoring submodules reads clean
/// whatever stands at one, a gitlink just added included. So a `/workdir` whose
/// index records one reads unknown, and the status is never run on it.
/// Submodules stay ignored in the status read regardless, so a gitlink staged
/// between the two reads is missed rather than entered.
fn dirty_without_entering_a_submodule(
    git: impl Fn(&str, &[&str]) -> Result<String, HortError>,
) -> Result<bool, HortError> {
    let staged = git("ls-files", &["ls-files", "--stage"])?;
    if staged.lines().any(|entry| entry.starts_with("160000 ")) {
        return Err(failed(
            "status",
            "the /workdir records a submodule, which the host does not enter",
        ));
    }
    let porcelain = git("status", &["status", "--porcelain", "--ignore-submodules=all"])?;
    Ok(!porcelain.trim().is_empty())
}

/// Whether the linked worktree at `worktree` holds uncommitted changes, read
/// through `admin`, the administrative directory `project` keeps for it on the
/// host, never through the worktree's own `.git` pointer.
///
/// The index that says whether the worktree records a submodule lives in that
/// directory, which the sandbox cannot write, so every submodule the status
/// could enter is in sight before the status runs. The worktree's own files,
/// `.gitmodules` included, are the sandbox's to write and to remove, so none of
/// them is asked. What the sandbox does write is whatever stands at a
/// submodule's path, which is why a worktree recording one reads unknown even
/// while nothing stands there yet. The status read ignores submodules as well,
/// because the index can still change from the host side between the two
/// reads: a gitlink the user stages in that moment is missed rather than
/// entered.
fn worktree_is_dirty(
    worktree: &Path,
    admin: &Path,
    project: &Path,
    deadline: Instant,
) -> Result<bool, HortError> {
    let admin_arg = admin.to_string_lossy();
    let worktree_arg = worktree.to_string_lossy();
    dirty_without_entering_a_submodule(|op, args| {
        let through_admin = ["--git-dir", admin_arg.as_ref(), "--work-tree", worktree_arg.as_ref()];
        run_by(project, op, &[&through_admin[..], args].concat(), deadline)
    })
}

/// Whether the clone at `clone` holds uncommitted changes, read from the host
/// without obeying anything the box wrote.
///
/// The clone's `.git` is a directory the box writes, configuration included,
/// and a status run there runs whatever that configuration names, such as an
/// fsmonitor or a filter bound to a tracked file. So the clone is read as data
/// only, its index and its objects, through a git directory of hort's own that
/// holds nothing but the commit the clone stands on and no configuration at
/// all. The system and global configuration are shut out too: a filter driver
/// the user defined globally would otherwise run on content the box wrote, as
/// soon as an attributes file the box also wrote names it. Only the tip is
/// resolved inside the clone, the way `holds_unreturned_work` resolves it,
/// because resolving a ref runs nothing the configuration names.
///
/// The objects the clone borrows are lent from `project`, the store the box
/// gets by mount, because the path the clone records is one only the box has.
fn clone_is_dirty(
    clone: &Path,
    sandbox_dir: &Path,
    project: &Path,
    deadline: Instant,
) -> Result<bool, HortError> {
    let tip = run_by(clone, "rev-parse", &["rev-parse", "HEAD"], deadline)?;
    let read = CloneRead::begin(sandbox_dir, tip.trim(), clone, project, deadline)?;
    dirty_without_entering_a_submodule(|op, args| read.git(op, args))
}

/// One read of a clone, through a git directory hort writes for it and removes
/// when the read ends, however it ends. That directory lives in the sandbox's
/// own state directory, beside the clone and never inside it, because the box
/// cannot write there, and it carries the reading process's id because two
/// listings can read the same clone at once.
struct CloneRead<'a> {
    git_dir: PathBuf,
    clone: &'a Path,
    project: &'a Path,
    deadline: Instant,
}

impl<'a> CloneRead<'a> {
    fn begin(
        sandbox_dir: &Path,
        tip: &str,
        clone: &'a Path,
        project: &'a Path,
        deadline: Instant,
    ) -> Result<Self, HortError> {
        let unwritable = |err: io::Error| failed("clone status directory", &err);
        let git_dir = sandbox_dir.join(format!("clone-status-{}", std::process::id()));
        std::fs::create_dir(&git_dir).map_err(unwritable)?;
        let read = Self { git_dir, clone, project, deadline };
        std::fs::create_dir(read.git_dir.join("refs")).map_err(unwritable)?;
        std::fs::write(read.git_dir.join("HEAD"), format!("{tip}\n")).map_err(unwritable)?;
        Ok(read)
    }

    /// Run git against the clone through this read's git directory and require
    /// success. Optional locks are off, because the index is the box's and a
    /// refresh written back from the host would take the lock the box's own git
    /// needs.
    fn git(&self, op: &str, args: &[&str]) -> Result<String, HortError> {
        let dot_git = self.clone.join(".git");
        let mut git = Command::new("git");
        git.arg("--no-optional-locks")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(self.clone)
            .args(args)
            .env("GIT_INDEX_FILE", dot_git.join("index"))
            .env("GIT_OBJECT_DIRECTORY", dot_git.join("objects"))
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", self.project.join(".git").join("objects"))
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        stdout_of_success(op, output_by(&mut git, op, self.deadline)?)
    }
}

impl Drop for CloneRead<'_> {
    fn drop(&mut self) {
        // Nothing is left to do with a failure here: whatever stays behind goes
        // with the rest of the sandbox's state when the sandbox is removed.
        let _ = std::fs::remove_dir_all(&self.git_dir);
    }
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

    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime};

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

    /// How long a read of a sandbox's `/workdir` may take before whoever asked
    /// for it would call hort hung. It bounds how long these tests wait, never
    /// how long a read is allowed to take: a read still waiting on the sandbox
    /// after this long was not going to come back by itself.
    const WHAT_A_CALLER_WAITS: Duration = Duration::from_secs(10);

    /// A file in a directory the sandbox writes, replaced by a named pipe that
    /// nothing writes to. Whatever opens it for reading waits until something
    /// opens the other end, which is all a sandbox needs to hold a read the host
    /// makes there for as long as it likes.
    struct PlantedPipe {
        path: PathBuf,
    }

    impl PlantedPipe {
        fn plant(path: &Path) -> Self {
            let _ = fs::remove_file(path);
            let spelled = CString::new(path.as_os_str().as_bytes()).unwrap();
            let made = unsafe { libc::mkfifo(spelled.as_ptr(), 0o644) };
            assert_eq!(made, 0, "no pipe planted at {}", path.display());
            Self { path: path.to_path_buf() }
        }

        /// Answer whatever is waiting to read the pipe with an end of file, and
        /// say whether anything was. Opening the writing end without blocking
        /// succeeds only while a reader holds the other end or waits for it.
        fn answer_a_waiting_reader(&self) -> bool {
            fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
                .is_ok()
        }
    }

    impl Drop for PlantedPipe {
        fn drop(&mut self) {
            // A reader still waiting when its directory is removed waits forever,
            // because nothing can open a pipe that is no longer there. So every
            // reader is answered first, a read's leftover included, and so is one
            // that opens the pipe again right after its answer.
            let mut quiet = 0;
            while quiet < 20 {
                quiet = if self.answer_a_waiting_reader() { 0 } else { quiet + 1 };
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Run `read`, and once a caller would have given up on it, answer every
    /// reader of `pipe` until it comes back, so a read waiting on the pipe fails
    /// its own test instead of holding the whole suite. Returns what the read
    /// returned and whether it came back by itself, before anything had to be
    /// answered for it.
    fn read_watched<T: Send>(pipe: &PlantedPipe, read: impl FnOnce() -> T + Send) -> (T, bool) {
        std::thread::scope(|scope| {
            let reading = scope.spawn(read);
            let given_up_at = Instant::now() + WHAT_A_CALLER_WAITS;
            let mut answered = false;
            while !reading.is_finished() {
                if Instant::now() >= given_up_at {
                    answered |= pipe.answer_a_waiting_reader();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let returned = reading.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (returned, !answered)
        })
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

    /// Write into `dir` an executable script whose only effect is creating
    /// `marker`, and return its path. It stands in for anything a sandbox can
    /// configure git to run, so the marker existing is that code having run on
    /// the host.
    fn planted_script(dir: &Path, marker: &Path) -> PathBuf {
        let script = dir.join("run-on-read.sh");
        fs::write(&script, format!("#!/bin/sh\ntouch {}\nprintf '%s\\0' 1\n", marker.display()))
            .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Give the file at `path` a modification time no checkout ever left on it,
    /// so git can no longer vouch for it from what it recorded last time and has
    /// to look at the file itself.
    fn set_a_modification_time_of_its_own(path: &Path) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();
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

        provider.remove(&name, Some(&repo)).unwrap();

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

        assert!(provider.remove(&name, Some(&repo)).is_ok());
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

        provider.remove(&name, Some(&repo)).unwrap();

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
        assert!(provider.remove(&name, Some(&repo)).is_ok());
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

        provider.remove(&name, Some(&repo)).unwrap();

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

        provider.remove(&name, Some(&repo)).unwrap();

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

        provider.remove(&name, Some(&repo)).unwrap();

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

        assert!(!provider.is_dirty(&name, &repo).unwrap());
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

        assert!(provider.is_dirty(&name, &repo).unwrap());
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

        assert!(provider.is_dirty(&name, &repo).unwrap());
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

        let result = provider.is_dirty(&name, &repo);

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

        let _ = provider.is_dirty(&name, &repo);

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

        assert!(!provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_asks_the_project_it_names_rather_than_the_repository_it_runs_from() {
        let (_project, project) = temp_dir();
        let (_elsewhere, elsewhere) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&project);
        init_repo_with_commit(&elsewhere);
        let name = SandboxName::new("demo").unwrap();
        let worktree = GitWorktreeProvider::new(project.clone(), state_root.clone())
            .create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree)
            .unwrap();
        fs::write(worktree.path.join("untracked.txt"), "x\n").unwrap();
        let run_from_elsewhere = GitWorktreeProvider::new(elsewhere, state_root);

        // `ls` and `prune` are global, so the repository a provider is rooted
        // at is whichever one the user is standing in, and that one keeps no
        // administrative directory for another project's worktree. Asked there,
        // every box of another project reads as unknown: a dash in `ls` and a
        // false skip in `prune`.
        assert!(run_from_elsewhere.is_dirty(&name, &project).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_gives_up_on_an_ignore_file_that_never_answers() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
        // The sandbox writes its whole worktree, and a status read opens every
        // ignore file in it.
        let planted = PlantedPipe::plant(&worktree.path.join(".gitignore"));

        let (read, came_back_by_itself) =
            read_watched(&planted, || provider.is_dirty(&name, &repo));

        assert!(came_back_by_itself, "the read was still waiting on the sandbox: {read:?}");
        // Answered "clean", a read that never finished lets `prune` collect a
        // worktree nobody looked at.
        assert!(
            matches!(read, Err(HortError::GitCommandFailed { .. })),
            "a read that could not finish answered as if it had: {read:?}"
        );
    }

    #[test]
    fn git_worktree_is_dirty_leaves_no_git_waiting_on_the_worktree_once_it_gives_up() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
        let planted = PlantedPipe::plant(&worktree.path.join(".gitignore"));

        let (read, came_back_by_itself) =
            read_watched(&planted, || provider.is_dirty(&name, &repo));

        // hort exits right after it lists, so a git it stopped waiting for is
        // not stopped with it: it stays behind, waiting on the sandbox for good.
        assert!(came_back_by_itself, "only a read that gave up can leave a git behind: {read:?}");
        assert!(
            !planted.answer_a_waiting_reader(),
            "a git the read started still waits on the pipe"
        );
    }

    #[test]
    fn git_worktree_is_dirty_reports_true_for_a_worktree_holding_thousands_of_untracked_files() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
        // Two thousand untracked files with long names make a status of over two
        // hundred kilobytes, several times what a pipe holds. Git stops writing
        // until somebody reads, so a read that only waits for git to end never
        // sees it end, and gives up on a worktree that is simply busy.
        for ordinal in 0..2000 {
            let file = format!("untracked-{}-{ordinal:05}.txt", "x".repeat(90));
            fs::write(worktree.path.join(file), "").unwrap();
        }

        assert!(provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn git_worktree_is_dirty_cannot_answer_for_a_worktree_of_a_project_with_a_submodule() {
        let (_repo, repo) = temp_dir();
        let (_upstream, upstream) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&upstream);
        init_repo_with_commit(&repo);
        // git takes a submodule's source from what a repository says rather
        // than from whoever runs it, so it refuses a local path unless told
        // otherwise. Both repositories here are this test's own.
        git(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                upstream.to_str().unwrap(),
                "vendored",
            ],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=hort-test",
                "-c",
                "user.email=hort-test@localhost",
                "commit",
                "-m",
                "vendor",
            ],
        );
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();

        let read = provider.is_dirty(&name, &repo);

        // Nothing stands at the submodule's path yet, but whatever comes to
        // stand there is the sandbox's to write, and a status read runs git
        // inside a submodule it finds checked out under the configuration that
        // checkout carries. A read that answers until then leaves to the
        // sandbox whether the host looks inside, so a project with a submodule
        // reads unknown however its worktree stands.
        assert!(read.is_err(), "a read that cannot look inside the submodule answered: {read:?}");
    }

    #[test]
    fn git_worktree_is_dirty_cannot_answer_for_a_worktree_holding_a_gitlink() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let worktree =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Worktree).unwrap();
        let nested = worktree.path.join("vendored");
        fs::create_dir(&nested).unwrap();
        init_repo_with_commit(&nested);
        git(&worktree.path, &["add", "vendored"]);

        let read = provider.is_dirty(&name, &repo);

        // Staged on the host and declared nowhere else: no commit records this
        // submodule and no `.gitmodules` names it, so only the index the host
        // keeps for the worktree says it is there. The worktree's own files
        // are the sandbox's to write and to remove, so they cannot be what
        // decides whether a submodule is there.
        assert!(read.is_err(), "a read that cannot look inside the submodule answered: {read:?}");
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
    fn clone_mode_gives_up_asking_for_unreturned_work_when_the_head_of_the_clone_never_answers() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        // The clone's `.git` is the sandbox's to write, and the tip is resolved
        // there, through the file that names it.
        let planted = PlantedPipe::plant(&workdir.path.join(".git").join("HEAD"));

        let (read, came_back_by_itself) =
            read_watched(&planted, || provider.holds_unreturned_work(&name, &repo));

        assert!(came_back_by_itself, "the read was still waiting on the sandbox: {read:?}");
        assert!(
            matches!(read, Err(HortError::GitCommandFailed { .. })),
            "a read that could not finish answered as if it had: {read:?}"
        );
    }

    #[test]
    fn clone_mode_is_dirty_reports_false_for_a_clean_clone() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();

        assert!(!provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_is_dirty_reports_true_for_a_modified_tracked_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        fs::write(workdir.path.join("README.md"), "changed\n").unwrap();

        assert!(provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_is_dirty_reports_true_for_an_untracked_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        fs::write(workdir.path.join("untracked.txt"), "x\n").unwrap();

        assert!(provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_is_dirty_reports_false_for_a_clone_standing_on_a_commit_only_it_holds() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        commit_in_clone(&workdir.path, &repo.join(".git").join("objects"), "work.txt");

        // A box that committed its work is clean, and the commit it stands on
        // exists in the clone's own object store and nowhere on the host. That
        // is the state of a box between two tasks, and a read that looks for
        // the commit only among the host's objects cannot answer for it.
        assert!(!provider.is_dirty(&name, &repo).unwrap());
    }

    #[test]
    fn clone_mode_is_dirty_asks_the_project_it_names_rather_than_the_repository_it_runs_from() {
        let (_project, project) = temp_dir();
        let (_elsewhere, elsewhere) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&project);
        // With no commit of its own: two repositories seeded alike within the
        // same second hold the very same commit, and this one must not hold the
        // commit the clone stands on.
        git(&elsewhere, &["init", "-b", "main"]);
        let name = SandboxName::new("demo").unwrap();
        GitWorktreeProvider::new(project.clone(), state_root.clone())
            .create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone)
            .unwrap();
        let run_from_elsewhere = GitWorktreeProvider::new(elsewhere, state_root);

        // `ls` and `prune` are global, so the repository a provider is rooted
        // at is whichever one the user is standing in. The commit a fresh clone
        // stands on is one only its own project holds, so asked of any other
        // repository the clone cannot even be read.
        assert!(!run_from_elsewhere.is_dirty(&name, &project).unwrap());
    }

    #[test]
    fn clone_mode_is_dirty_cannot_answer_for_a_clone_holding_a_gitlink() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        let nested = workdir.path.join("vendored");
        fs::create_dir(&nested).unwrap();
        init_repo_with_commit(&nested);
        let staged = git_borrowing_objects(
            &workdir.path,
            &repo.join(".git").join("objects"),
            &["add", "vendored"],
        );
        assert!(staged.status.success(), "git add of the nested repository failed");

        // A gitlink leaves two readings and both are wrong. Looking inside the
        // submodule runs git there under the configuration the box wrote for
        // it, and looking away from it hides a gitlink the box just added, so
        // this clone reads clean. Unknown is the one answer that does neither.
        assert!(provider.is_dirty(&name, &repo).is_err());
    }

    #[test]
    fn clone_mode_is_dirty_does_not_run_an_fsmonitor_the_clone_configures() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();

        // The clone's `.git` is a directory the box writes, configuration
        // included, and a status read consults the fsmonitor that configuration
        // names on every run.
        let marker = state_root.join("agent-code-ran");
        let hook = planted_script(&workdir.path.join(".git"), &marker);
        git(&workdir.path, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

        let read = provider.is_dirty(&name, &repo);

        assert!(read.is_ok(), "only a read that reached git can say what git ran: {read:?}");
        assert!(!marker.exists(), "the fsmonitor the box configured ran on the host");
    }

    #[test]
    fn clone_mode_is_dirty_does_not_run_a_clean_filter_the_clone_binds_to_a_tracked_file() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();

        // The box defines a filter in the configuration it writes and binds it
        // to a tracked file through an attributes file in its own worktree. The
        // file is rewritten at its own size, so its size cannot say it changed
        // and git has to read it through the filter to compare it.
        let marker = state_root.join("agent-code-ran");
        git(
            &workdir.path,
            &["config", "filter.planted.clean", &format!("touch {}; cat", marker.display())],
        );
        fs::write(workdir.path.join(".gitattributes"), "README.md filter=planted\n").unwrap();
        fs::write(workdir.path.join("README.md"), "SEED\n").unwrap();
        set_a_modification_time_of_its_own(&workdir.path.join("README.md"));

        let read = provider.is_dirty(&name, &repo);

        assert!(read.is_ok(), "only a read that reached git can say what git ran: {read:?}");
        assert!(!marker.exists(), "the filter the box configured ran on the host");
    }

    #[test]
    fn clone_mode_is_dirty_does_not_run_an_fsmonitor_a_submodule_of_the_clone_configures() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        let nested = workdir.path.join("vendored");
        fs::create_dir(&nested).unwrap();
        init_repo_with_commit(&nested);
        let staged = git_borrowing_objects(
            &workdir.path,
            &repo.join(".git").join("objects"),
            &["add", "vendored"],
        );
        assert!(staged.status.success(), "git add of the nested repository failed");

        // A repository nested in the clone and recorded in its index is a
        // submodule, and a status read of the clone runs git inside it under the
        // submodule's own configuration, which the box wrote as well.
        let marker = state_root.join("agent-code-ran");
        let hook = planted_script(&nested.join(".git"), &marker);
        git(&nested, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

        let _ = provider.is_dirty(&name, &repo);

        assert!(!marker.exists(), "the fsmonitor the box gave its submodule ran on the host");
    }

    #[test]
    fn clone_mode_is_dirty_leaves_the_index_of_the_clone_as_it_found_it() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        // Touched without being changed, so a read that refreshes what the index
        // remembers about the file has something to write back. The index is
        // the box's, and a write into it from the host takes the lock the box's
        // own git needs to work.
        set_a_modification_time_of_its_own(&workdir.path.join("README.md"));
        let index = workdir.path.join(".git").join("index");
        let before = fs::read(&index).unwrap();

        let read = provider.is_dirty(&name, &repo);

        assert!(read.is_ok(), "only a read that reached git can say what git wrote: {read:?}");
        assert!(fs::read(&index).unwrap() == before, "the read wrote the clone's index");
    }

    #[test]
    fn clone_mode_is_dirty_gives_up_when_the_head_of_the_clone_never_answers() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        // The read starts from the commit the clone stands on, which only the
        // clone's own `.git` can name.
        let planted = PlantedPipe::plant(&workdir.path.join(".git").join("HEAD"));

        let (read, came_back_by_itself) =
            read_watched(&planted, || provider.is_dirty(&name, &repo));

        assert!(came_back_by_itself, "the read was still waiting on the sandbox: {read:?}");
        assert!(
            matches!(read, Err(HortError::GitCommandFailed { .. })),
            "a read that could not finish answered as if it had: {read:?}"
        );
    }

    #[test]
    fn clone_mode_is_dirty_gives_up_when_the_index_of_the_clone_never_answers() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        let workdir =
            provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();
        // Past the commit it stands on, the clone is read as data, its index
        // first, by a git that is not the one that named the commit. Either can
        // be left waiting, and a HEAD that never answers stops the read before
        // the second is ever started.
        let planted = PlantedPipe::plant(&workdir.path.join(".git").join("index"));

        let (read, came_back_by_itself) =
            read_watched(&planted, || provider.is_dirty(&name, &repo));

        assert!(came_back_by_itself, "the read was still waiting on the sandbox: {read:?}");
        assert!(
            matches!(read, Err(HortError::GitCommandFailed { .. })),
            "a read that could not finish answered as if it had: {read:?}"
        );
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

        provider.remove(&name, Some(&repo)).unwrap();

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

        provider.remove(&going, Some(&repo)).unwrap();

        // The pin is what keeps the host's gc off the objects a clone borrows
        // rather than owns, so collecting one box by sweeping every pin leaves
        // the boxes still standing one `git gc` away from an unreadable
        // `/workdir`.
        assert_eq!(rev_parse(&repo, "refs/hort/other/base"), base);
    }

    #[test]
    fn clone_mode_remove_deletes_the_pinned_base_ref_from_the_project_it_names() {
        let (_project, project) = temp_dir();
        let (_elsewhere, elsewhere) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&project);
        init_repo_with_commit(&elsewhere);
        let name = SandboxName::new("demo").unwrap();
        GitWorktreeProvider::new(project.clone(), state_root.clone())
            .create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone)
            .unwrap();
        let run_from_elsewhere = GitWorktreeProvider::new(elsewhere, state_root);

        run_from_elsewhere.remove(&name, Some(&project)).unwrap();

        // `down` and `prune` run from wherever the user stands. Released in that
        // repository, the pin stays in the project, holding a commit for a
        // sandbox no hort command will ever name again.
        let pinned = Command::new("git")
            .current_dir(&project)
            .args(["rev-parse", "--verify", "--quiet", "refs/hort/demo/base"])
            .output()
            .unwrap();
        assert!(!pinned.status.success(), "the pinned base ref outlived the sandbox");
    }

    #[test]
    fn clone_mode_remove_without_a_project_deletes_the_pinned_base_ref_where_it_runs() {
        let (_repo, repo) = temp_dir();
        let (_state, state_root) = temp_dir();
        init_repo_with_commit(&repo);
        let provider = GitWorktreeProvider::new(repo.clone(), state_root.clone());
        let name = SandboxName::new("demo").unwrap();
        provider.create(&name, &BranchName::new("demo").unwrap(), GitMode::Clone).unwrap();

        provider.remove(&name, None).unwrap();

        // A corrupt entry has lost the record that named its project. Run from
        // that project, releasing the pin there is what hort did before it
        // could name one, and a sandbox that cannot say where it came from is
        // no reason to leak the ref in the one repository that holds it.
        let pinned = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "--verify", "--quiet", "refs/hort/demo/base"])
            .output()
            .unwrap();
        assert!(!pinned.status.success(), "the pinned base ref outlived the sandbox");
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
