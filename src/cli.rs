//! CLI surface: the clap v4 derive definitions for the subcommands hort exposes,
//! their dispatch, and the pure `ls`, `prune` and warning renderers.
//!
//! Only subcommands that work end to end ship here. This slice is `up`, `ls`,
//! `down` and `prune`; `attach`, `config` and `doctor` arrive with the tasks
//! that make them real, so the binary never offers a command that cannot run.

use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::adapters::clock::SystemClock;
use crate::adapters::config::{ConfigResolver, find_project_dir};
use crate::adapters::confirm::StdinConfirmer;
use crate::adapters::environment::HostEnvironmentProbe;
use crate::adapters::liveness::ProcLivenessProbe;
use crate::adapters::lock::FlockSandboxLock;
use crate::adapters::metadata::FileMetadataStore;
use crate::adapters::pasta::PastaNetworkProvider;
use crate::adapters::runtime::{LibcontainerRuntime, NullRuntime};
use crate::adapters::worktree::GitWorktreeProvider;
use crate::commands::down::DownCommand;
use crate::commands::ls::{LsCommand, LsEntry};
use crate::commands::prune::{PruneCommand, PruneReport};
use crate::commands::up::UpCommand;
use crate::domain::error::HortError;
use crate::domain::idle::IdleState;
use crate::domain::model::{BranchName, SandboxName, Warning};
use crate::domain::prune::SkipReason;
use crate::domain::reconcile::SandboxState;

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
    /// Build a sandbox and return to the prompt.
    Up {
        /// The sandbox to build, which is also the branch it creates.
        name: String,
        /// Check out this existing branch instead of creating one named after
        /// the sandbox.
        #[arg(long)]
        branch: Option<String>,
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
    clock: SystemClock,
    confirmer: StdinConfirmer,
    env: HostEnvironmentProbe,
    config: ConfigResolver,
    /// Kept so `prune` can derive a corrupt entry's canonical worktree path,
    /// which has no record to read it from.
    state_root: PathBuf,
    /// The project a marker declares, `None` when nothing declares one.
    project_dir: Option<PathBuf>,
    /// Where hort was invoked, which is the directory a refusal names.
    current_dir: PathBuf,
}

impl RealDeps {
    /// Resolve the state root and project directory and wire the real adapters.
    /// The state root is created if missing, and both it and the project
    /// directory are canonicalized so a symlinked root cannot make a record's
    /// stored worktree path disagree with the path git reports.
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

        Ok(Self {
            lock: FlockSandboxLock::new(state_root.clone()),
            store: FileMetadataStore::new(state_root.clone()),
            probe: ProcLivenessProbe,
            worktrees: GitWorktreeProvider::new(adapters_dir.clone(), state_root.clone()),
            runtime: LibcontainerRuntime::new(resolve_youki_root(), state_root.clone()),
            sessions: NullRuntime,
            network: PastaNetworkProvider::new(state_root.clone()),
            clock: SystemClock,
            confirmer: StdinConfirmer,
            env: HostEnvironmentProbe,
            config: ConfigResolver::new(resolve_config_root()?, adapters_dir, home_dir()?),
            state_root,
            project_dir,
            current_dir,
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

/// The directory the embedded runtime keeps its container state in:
/// `$XDG_RUNTIME_DIR/hort` when that variable names a directory, otherwise
/// `/run/user/<uid>/hort`.
///
/// Both are emptied when the machine restarts, and that is the point rather than
/// tidiness. The runtime refuses to build a container whose id it already has
/// state for, so state that outlived the anchor it describes would make
/// `up <name>` fail for good after a crash, leaving no way back but deleting
/// files by hand. Keeping it under hort's own state would do exactly that.
fn resolve_youki_root() -> PathBuf {
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
pub fn run(cli: Cli, deps: &RealDeps) -> Result<(), HortError> {
    match cli.command {
        CliCommand::Up { name, branch } => {
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
                deps.state_root.clone(),
                deps.project_dir.clone(),
                deps.current_dir.clone(),
                &config,
            );
            let warnings = command.run(name, branch)?;
            eprint!("{}", render_warnings(&config_warnings, &warnings));
            Ok(())
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
            Ok(())
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
            command.run(name, force, std::io::stdin().is_terminal())
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
            Ok(())
        }
    }
}

const DASH: &str = "-";

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
