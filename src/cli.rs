//! CLI surface: the clap v4 derive definitions for the subcommands hort exposes,
//! their dispatch, and the pure `ls`, `prune` and warning renderers.
//!
//! Only subcommands that work end to end ship here. This slice is `up`, `attach`,
//! `ls`, `down` and `prune`; `config` and `doctor` arrive with the tasks that
//! make them real, so the binary never offers a command that cannot run.
//!
//! A run that opened a session leaves with the status that session exited with,
//! which is what lets a script tell what ran inside a sandbox from what hort
//! itself did. That collides with hort's own exit codes, the same trade `ssh`
//! makes.

use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::adapters::cache::FileCacheProvider;
use crate::adapters::clock::SystemClock;
use crate::adapters::config::{ConfigResolver, find_project_dir};
use crate::adapters::confirm::StdinConfirmer;
use crate::adapters::environment::HostEnvironmentProbe;
use crate::adapters::liveness::ProcLivenessProbe;
use crate::adapters::lock::FlockSandboxLock;
use crate::adapters::metadata::FileMetadataStore;
use crate::adapters::pasta::PastaNetworkProvider;
use crate::adapters::runtime::{LibcontainerRuntime, NullRuntime};
use crate::adapters::terminal::HostTerminal;
use crate::adapters::worktree::GitWorktreeProvider;
use crate::commands::attach::AttachCommand;
use crate::commands::down::DownCommand;
use crate::commands::ls::{LsCommand, LsEntry};
use crate::commands::prune::{PruneCommand, PruneReport};
use crate::commands::up::UpCommand;
use crate::domain::config::ResolvedConfig;
use crate::domain::error::HortError;
use crate::domain::idle::IdleState;
use crate::domain::model::{BranchName, SandboxName, Warning};
use crate::domain::prune::SkipReason;
use crate::domain::reconcile::SandboxState;
use crate::ports::SessionTerminal;

/// The parsed command line: one subcommand and its flags.
#[derive(Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

/// The subcommands this build exposes. The set grows as each command becomes able
/// to run for real.
#[derive(Subcommand)]
pub enum CliCommand {
    /// Build a sandbox and open a session in it.
    Up {
        /// The sandbox to build, which is also the branch it creates.
        name: String,
        /// Check out this existing branch instead of creating one named after
        /// the sandbox.
        #[arg(long)]
        branch: Option<String>,
        /// Return to the prompt with the sandbox running instead of opening a
        /// session in it.
        #[arg(short, long)]
        detach: bool,
    },
    /// Open one more session in a running sandbox.
    Attach {
        /// The sandbox to join.
        name: String,
    },
    /// List every sandbox with its reconciled state.
    Ls,
    /// Tear a sandbox down in the mandatory order.
    Down {
        /// The sandbox to tear down.
        name: String,
        /// Skip the open-sessions confirmation.
        #[arg(short, long)]
        force: bool,
    },
    /// Remove idle sandboxes and abrupt-death debris after confirming.
    Prune {
        /// Skip the confirmation prompt and the dirty exclusion.
        #[arg(short, long)]
        force: bool,
        /// Also remove sandboxes idle at least this long.
        #[arg(long, value_parser = humantime::parse_duration)]
        idle: Option<Duration>,
    },
}

/// The real adapters the commands run against, assembled once at startup. The
/// per-sandbox process list is the one read still served by a placeholder, so a
/// sandbox reads as having no session open.
pub struct RealDeps {
    lock: FlockSandboxLock,
    store: FileMetadataStore,
    probe: ProcLivenessProbe,
    worktrees: GitWorktreeProvider,
    runtime: LibcontainerRuntime,
    sessions: NullRuntime,
    network: PastaNetworkProvider,
    terminal: HostTerminal,
    clock: SystemClock,
    confirmer: StdinConfirmer,
    env: HostEnvironmentProbe,
    cache: FileCacheProvider,
    config: ConfigResolver,
    /// Kept so `prune` can derive a corrupt entry's canonical worktree path,
    /// which has no record to read it from.
    state_root: PathBuf,
    /// The project a marker declares, `None` when nothing declares one.
    project_dir: Option<PathBuf>,
    /// Where hort was invoked, which is the directory a refusal names.
    current_dir: PathBuf,
    /// The home the user has on this host, which is what a declared mount path
    /// is measured against before it is carried into the sandbox.
    host_home: PathBuf,
}

impl RealDeps {
    /// Resolve the two roots and the project directory and wire the real
    /// adapters. The state root is created if missing, and both it and the
    /// project directory are canonicalized so a symlinked root cannot make a
    /// record's stored worktree path disagree with the path git reports.
    ///
    /// Which root an adapter is handed is decided here and nowhere else: the ones
    /// that keep a record of the sandbox get the state root, the ones whose files
    /// mean nothing after a restart get the runtime root.
    ///
    /// Building the configuration reader reads nothing. Configuration is a
    /// precondition of building a sandbox and of nothing else, so a project whose
    /// configuration hort cannot parse still lists and tears down what it has.
    pub fn assemble() -> Result<Self, HortError> {
        let state_root = resolve_state_root()?;
        fs::create_dir_all(&state_root).map_err(|error| HortError::StateIo {
            detail: format!("could not create {}: {error}", state_root.display()),
        })?;
        let state_root = state_root.canonicalize().map_err(|error| HortError::StateIo {
            detail: format!("could not resolve {}: {error}", state_root.display()),
        })?;

        let current_dir = std::env::current_dir()
            .map_err(|error| HortError::StateIo {
                detail: format!("could not read the current directory: {error}"),
            })?
            .canonicalize()
            .map_err(|error| HortError::StateIo {
                detail: format!("could not resolve the current directory: {error}"),
            })?;
        // One notion of where the project is, so what the configuration is read
        // from and what the worktrees are cut from cannot come to disagree. With
        // no project at all the adapters still need a directory to point at, and
        // what refuses to build on an unmarked one is the command.
        let project_dir = find_project_dir(&current_dir);
        let adapters_dir = project_dir.clone().unwrap_or_else(|| current_dir.clone());
        let host_home = home_dir()?;
        let runtime_root = resolve_runtime_root();

        Ok(Self {
            lock: FlockSandboxLock::new(state_root.clone()),
            store: FileMetadataStore::new(state_root.clone()),
            probe: ProcLivenessProbe,
            worktrees: GitWorktreeProvider::new(adapters_dir.clone(), state_root.clone()),
            runtime: LibcontainerRuntime::new(runtime_root.clone()),
            sessions: NullRuntime,
            network: PastaNetworkProvider::new(runtime_root),
            terminal: HostTerminal,
            clock: SystemClock,
            confirmer: StdinConfirmer,
            env: HostEnvironmentProbe,
            cache: FileCacheProvider,
            config: ConfigResolver::new(resolve_config_root()?, adapters_dir, host_home.clone()),
            state_root,
            project_dir,
            current_dir,
            host_home,
        })
    }
}

/// The directory hort keeps its per-sandbox records under: `$XDG_STATE_HOME/hort`
/// when that variable names a directory, otherwise the XDG default of
/// `~/.local/state/hort`.
fn resolve_state_root() -> Result<PathBuf, HortError> {
    match xdg_hort_dir("XDG_STATE_HOME") {
        Some(root) => Ok(root),
        None => Ok(home_dir()?.join(".local").join("state").join("hort")),
    }
}

/// The directory hort reads its global configuration from: `$XDG_CONFIG_HOME/hort`
/// when that variable names a directory, otherwise the XDG default of
/// `~/.config/hort`.
fn resolve_config_root() -> Result<PathBuf, HortError> {
    match xdg_hort_dir("XDG_CONFIG_HOME") {
        Some(root) => Ok(root),
        None => Ok(home_dir()?.join(".config").join("hort")),
    }
}

/// The directory hort keeps everything a restart makes meaningless in: the
/// container states, and the files the host-side helpers of a sandbox write.
/// `$XDG_RUNTIME_DIR/hort` when that variable names a directory, otherwise
/// `/run/user/<uid>/hort`.
///
/// Both are emptied when the machine restarts, and that is the point rather than
/// tidiness. The runtime refuses to build a container whose id it already has
/// state for, so state that outlived the anchor it describes would make
/// `up <name>` fail for good after a crash, leaving no way back but deleting
/// files by hand. Keeping it under hort's own state would do exactly that, and
/// the helpers have a second reason: they exec binaries the distribution labels,
/// and the label a user's records carry refuses what those binaries write.
fn resolve_runtime_root() -> PathBuf {
    xdg_hort_dir("XDG_RUNTIME_DIR").unwrap_or_else(|| {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}")).join("hort")
    })
}

/// hort's own directory under the XDG base directory named by `variable`, when
/// the environment sets it to something. Honoring these variables is also the
/// seam the CLI tests use to keep off the real user state.
fn xdg_hort_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|base| !base.is_empty())
        .map(|base| PathBuf::from(base).join("hort"))
}

fn home_dir() -> Result<PathBuf, HortError> {
    std::env::home_dir().ok_or_else(|| HortError::StateIo {
        detail: "could not determine the home directory".to_string(),
    })
}

/// Dispatch a parsed command to its coordinator, printing what a script reads
/// (the `ls` rows, the `prune` report) to stdout and the advisories a build
/// raised to stderr. A returned error propagates to the binary, which prints it
/// once.
pub fn run(cli: Cli, deps: &RealDeps) -> Result<u8, HortError> {
    match cli.command {
        CliCommand::Up { name, branch, detach } => {
            let name = SandboxName::new(&name)?;
            let branch = branch.as_deref().map(BranchName::new).transpose()?;
            // Read here rather than at assembly: configuration is what a sandbox
            // is built out of, and a project whose configuration hort cannot
            // parse still has sandboxes to list and tear down.
            let (config, config_warnings) = deps.config.resolve()?;
            let command = UpCommand::new(
                &deps.lock,
                &deps.store,
                &deps.probe,
                &deps.runtime,
                &deps.worktrees,
                &deps.runtime,
                &deps.network,
                &deps.clock,
                &deps.env,
                &deps.cache,
                deps.state_root.clone(),
                deps.project_dir.clone(),
                deps.current_dir.clone(),
                deps.host_home.clone(),
                &config,
            );
            let warnings = command.run(name.clone(), branch)?;
            eprint!("{}", render_warnings(&config_warnings, &warnings));
            if detach {
                return Ok(HORT_SUCCEEDED);
            }
            // Composed rather than built into `up`, so that `hort up x` is worth
            // literally `hort up -d x` followed by `hort attach x`, one terminal
            // contract and one exit-status rule for both.
            open_session(deps, name, &config)
        }
        CliCommand::Attach { name } => {
            let name = SandboxName::new(&name)?;
            let (config, config_warnings) = deps.config.resolve()?;
            eprint!("{}", render_warnings(&config_warnings, &[]));
            open_session(deps, name, &config)
        }
        CliCommand::Ls => {
            let command = LsCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.worktrees,
                &deps.sessions,
                &deps.clock,
            );
            let entries = command.run()?;
            print!("{}", render_ls(&entries));
            Ok(HORT_SUCCEEDED)
        }
        CliCommand::Down { name, force } => {
            let name = SandboxName::new(&name)?;
            let command = DownCommand::new(
                &deps.store,
                &deps.sessions,
                &deps.confirmer,
                &deps.runtime,
                &deps.network,
                &deps.worktrees,
            );
            command.run(name, force, std::io::stdin().is_terminal())?;
            Ok(HORT_SUCCEEDED)
        }
        CliCommand::Prune { force, idle } => {
            let command = PruneCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.worktrees,
                &deps.sessions,
                &deps.clock,
                &deps.confirmer,
                &deps.runtime,
                &deps.network,
                deps.state_root.clone(),
            );
            let report = command.run(idle, force, std::io::stdin().is_terminal())?;
            print!("{}", render_prune(&report));
            Ok(HORT_SUCCEEDED)
        }
    }
}

/// Open a session in `name` and hold the terminal until it ends, reporting what
/// it exited with.
///
/// Whether there is a terminal to lend is decided here and nowhere else: it is a
/// fact about the process hort was invoked from, which the command it carries
/// into cannot see. Without a terminal there is no pty to allocate and nothing to
/// protect, so the session runs on the inherited streams instead of being
/// refused, which is what keeps hort usable from a script.
fn open_session(
    deps: &RealDeps,
    name: SandboxName,
    config: &ResolvedConfig,
) -> Result<u8, HortError> {
    let command = AttachCommand::new(&deps.store, &deps.probe, &deps.runtime, &deps.clock, config);
    let session = command.run(name, std::io::stdin().is_terminal())?;
    Ok(session_exit_code(deps.terminal.relay(session)?))
}

const DASH: &str = "-";

/// What hort leaves with when it ran a command of its own rather than a session.
const HORT_SUCCEEDED: u8 = 0;

/// What a shell adds to the signal number when it reports a process that was
/// killed rather than one that returned.
const SIGNALLED_EXIT_BASE: u8 = 128;

/// Render the `ls` rows for the terminal: one line per sandbox with its name,
/// lowercase state, session count, age, idle, branch, and worktree dirty state. A
/// figure with no value renders as a dash, and a sandbox with a running session
/// renders its idle as `active`.
pub fn render_ls(entries: &[LsEntry]) -> String {
    entries.iter().map(|entry| format!("{}\n", render_line(entry))).collect()
}

/// Render the `prune` report for the terminal: the names it removed and the names
/// it skipped with the reason for each. Layout is free; only the presence of the
/// names and reasons is a contract.
pub fn render_prune(report: &PruneReport) -> String {
    let removed = report.removed.iter().map(|name| format!("removed {name}\n"));
    let skipped = report
        .skipped
        .iter()
        .map(|skip| format!("skipped {} ({})\n", skip.name, skip_reason_label(&skip.reason)));
    removed.chain(skipped).collect()
}

/// Render the advisories a build produced for the terminal: what resolving the
/// configuration had to say, and what building the sandbox had to say. Layout is
/// free; only the presence of every advisory is a contract.
///
/// Both lists arrive here together because they are produced in different places
/// and a caller holding two of them prints one and drops the other. What is
/// dropped that way is a degradation hort promised to report: a resource ceiling
/// the host cannot enforce, or a configuration key it ignored.
pub fn render_warnings(config: &[Warning], command: &[Warning]) -> String {
    config.iter().chain(command).map(|warning| format!("warning: {warning}\n")).collect()
}

/// The code hort leaves with after a session it opened has ended, from the wait
/// status the kernel reported for it.
///
/// A caller has to be able to tell what ran inside the sandbox from what hort
/// itself did, and the only status a script knows how to read is the one its
/// shell would have produced.
pub fn session_exit_code(wait_status: i32) -> u8 {
    if libc::WIFSIGNALED(wait_status) {
        // The exit code carried in the wait status of a signalled process is
        // zero, so without the shell's rule a session the user interrupted with
        // ^C reports success to whatever script called hort.
        return SIGNALLED_EXIT_BASE + libc::WTERMSIG(wait_status) as u8;
    }
    libc::WEXITSTATUS(wait_status) as u8
}

fn render_line(entry: &LsEntry) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}  {}",
        entry.name.as_str(),
        state_label(entry.state),
        entry.sessions,
        render_duration(entry.age),
        render_idle(entry.idle.as_ref()),
        render_branch(entry.branch.as_ref()),
        render_dirty(entry.dirty),
    )
}

fn skip_reason_label(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::Dirty => "dirty",
        SkipReason::Unknown => "unknown",
    }
}

fn render_dirty(dirty: Option<bool>) -> String {
    match dirty {
        Some(true) => "dirty".to_string(),
        Some(false) => "clean".to_string(),
        None => DASH.to_string(),
    }
}

fn state_label(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Live => "live",
        SandboxState::Orphaned => "orphaned",
        SandboxState::LostRecord => "lost-record",
        SandboxState::Inconsistent => "inconsistent",
    }
}

fn render_duration(duration: Option<Duration>) -> String {
    match duration {
        Some(duration) => humantime::format_duration(duration).to_string(),
        None => DASH.to_string(),
    }
}

fn render_idle(idle: Option<&IdleState>) -> String {
    match idle {
        Some(IdleState::Active) => "active".to_string(),
        Some(IdleState::Idle(duration)) => humantime::format_duration(*duration).to_string(),
        None => DASH.to_string(),
    }
}

fn render_branch(branch: Option<&BranchName>) -> String {
    match branch {
        Some(branch) => branch.as_str().to_string(),
        None => DASH.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::domain::idle::IdleState;
    use crate::domain::model::{BranchName, SandboxName, Warning};
    use crate::domain::prune::{PruneSkip, SkipReason};
    use crate::domain::reconcile::SandboxState;

    #[test]
    fn render_ls_includes_each_required_column_for_entry() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: 2,
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("demo"));
        assert!(rendered.contains("live"));
        assert!(rendered.contains("2"));
        assert!(rendered.contains("1h"));
        assert!(rendered.contains("5m"));
        assert!(rendered.contains("clean"));
    }

    #[test]
    fn render_ls_renders_dashes_for_lost_record_row() {
        let entry = LsEntry {
            name: SandboxName::new("ghost").unwrap(),
            state: SandboxState::LostRecord,
            sessions: 0,
            age: None,
            idle: None,
            branch: None,
            dirty: None,
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("lost-record"));
        assert!(rendered.contains("-"));
    }

    #[test]
    fn render_ls_renders_running_sessions_as_active() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: 1,
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Active),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("active"));
    }

    #[test]
    fn render_prune_lists_removed_and_skipped() {
        let report = PruneReport {
            removed: vec!["demo".to_string()],
            skipped: vec![PruneSkip { name: "rotten".to_string(), reason: SkipReason::Dirty }],
        };

        let rendered = render_prune(&report);

        assert!(rendered.contains("demo"));
        assert!(rendered.contains("rotten"));
        assert!(rendered.contains("dirty"));
    }

    #[test]
    fn render_prune_reports_an_unknown_worktree_state_as_its_own_reason() {
        let report = PruneReport {
            removed: Vec::new(),
            skipped: vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }],
        };

        let rendered = render_prune(&report);

        // This line is what the user reads before deciding whether to pass
        // --force, and "dirty" would send them looking for uncommitted changes
        // in a worktree whose repository is gone.
        assert!(rendered.contains("unknown"));
    }

    #[test]
    fn render_warnings_reports_both_the_config_and_the_command_advisories() {
        let config = [Warning::new("ignoring devcontainer key 'image'")];
        let command = [Warning::new("memory limit dropped: controller not delegated")];

        let rendered = render_warnings(&config, &command);

        // The two lists come from two places, and printing one of them is the
        // silent degradation the whole advisory channel exists to prevent: a
        // sandbox running without the ceiling its config asked for looks exactly
        // like one running with it.
        assert!(rendered.contains("ignoring devcontainer key 'image'"));
        assert!(rendered.contains("memory limit dropped"));
    }

    #[test]
    fn session_exit_code_is_the_code_the_session_exited_with() {
        // The wait status of a process that called exit(7), which is the number
        // in the second byte.
        let exited_with_seven = 7 << 8;

        assert_eq!(session_exit_code(exited_with_seven), 7);
    }

    #[test]
    fn session_exit_code_of_a_killed_session_follows_the_shell_convention() {
        // The wait status of a process killed by SIGINT, which is the signal
        // number in the low byte and no exit code at all.
        let killed_by_sigint = 2;

        // Reading the exit code out of this status yields zero, so a session the
        // user interrupted would report to a script as one that finished its
        // work. Every shell answers 128 plus the signal here, and hort is read by
        // the same scripts.
        assert_eq!(session_exit_code(killed_by_sigint), 130);
    }

    #[test]
    fn render_ls_renders_dirty_state_for_dirty_entry() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: 0,
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(true),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("dirty"));
    }
}
