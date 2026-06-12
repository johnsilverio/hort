//! CLI surface: the clap v4 derive definitions for the subcommands hort exposes,
//! their dispatch, and the pure `ls` renderer.
//!
//! Only subcommands that work end to end ship here. This slice is `ls` and
//! `down`; `up`, `attach`, `prune`, `config`, and `doctor` arrive with the tasks
//! that make them real, so the binary never offers a command that cannot run.

use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::adapters::clock::SystemClock;
use crate::adapters::confirm::StdinConfirmer;
use crate::adapters::metadata::FileMetadataStore;
use crate::adapters::network::NullNetwork;
use crate::adapters::runtime::NullRuntime;
use crate::adapters::worktree::GitWorktreeProvider;
use crate::commands::down::DownCommand;
use crate::commands::ls::{LsCommand, LsEntry};
use crate::domain::error::HortError;
use crate::domain::idle::IdleState;
use crate::domain::model::{BranchName, SandboxName};
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
}

/// The real adapters the commands run against, assembled once at startup. The
/// container runtime and network are honest placeholders until their embedded
/// implementations land.
pub struct RealDeps {
    store: FileMetadataStore,
    worktrees: GitWorktreeProvider,
    runtime: NullRuntime,
    network: NullNetwork,
    clock: SystemClock,
    confirmer: StdinConfirmer,
}

impl RealDeps {
    /// Resolve the state root and project directory and wire the real adapters.
    /// The state root is created if missing, and both it and the project
    /// directory are canonicalized so a symlinked root cannot make a record's
    /// stored worktree path disagree with the path git reports.
    pub fn assemble() -> Result<Self, HortError> {
        let state_root = resolve_state_root()?;
        fs::create_dir_all(&state_root).map_err(|error| HortError::StateIo {
            detail: format!("could not create {}: {error}", state_root.display()),
        })?;
        let state_root = state_root.canonicalize().map_err(|error| HortError::StateIo {
            detail: format!("could not resolve {}: {error}", state_root.display()),
        })?;

        let repo_dir = std::env::current_dir()
            .map_err(|error| HortError::StateIo {
                detail: format!("could not read the current directory: {error}"),
            })?
            .canonicalize()
            .map_err(|error| HortError::StateIo {
                detail: format!("could not resolve the current directory: {error}"),
            })?;

        Ok(Self {
            store: FileMetadataStore::new(state_root.clone()),
            worktrees: GitWorktreeProvider::new(repo_dir, state_root),
            runtime: NullRuntime,
            network: NullNetwork,
            clock: SystemClock,
            confirmer: StdinConfirmer,
        })
    }
}

/// The directory hort keeps its per-sandbox records under: `$XDG_STATE_HOME/hort`
/// when that variable names a directory, otherwise the XDG default of
/// `~/.local/state/hort`. Honoring the variable is the seam the CLI tests use to
/// keep off the real user state.
fn resolve_state_root() -> Result<PathBuf, HortError> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("hort"));
        }
    }
    let home = std::env::home_dir().ok_or_else(|| HortError::StateIo {
        detail: "could not determine the home directory".to_string(),
    })?;
    Ok(home.join(".local").join("state").join("hort"))
}

/// Dispatch a parsed command to its coordinator, printing `ls` output to stdout.
/// A returned error propagates to the binary, which prints it once.
pub fn run(cli: Cli, deps: &RealDeps) -> Result<(), HortError> {
    match cli.command {
        CliCommand::Ls => {
            let command = LsCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.worktrees,
                &deps.runtime,
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
                &deps.runtime,
                &deps.confirmer,
                &deps.runtime,
                &deps.network,
                &deps.worktrees,
            );
            command.run(name, force, std::io::stdin().is_terminal())
        }
    }
}

const DASH: &str = "-";

/// Render the `ls` rows for the terminal: one line per sandbox with its name,
/// lowercase state, session count, age, idle, and branch. A figure with no value
/// renders as a dash, and a sandbox with a running session renders its idle as
/// `active`.
pub fn render_ls(entries: &[LsEntry]) -> String {
    entries.iter().map(|entry| format!("{}\n", render_line(entry))).collect()
}

fn render_line(entry: &LsEntry) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}",
        entry.name.as_str(),
        state_label(entry.state),
        entry.sessions,
        render_duration(entry.age),
        render_idle(entry.idle.as_ref()),
        render_branch(entry.branch.as_ref()),
    )
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
    use crate::domain::model::{BranchName, SandboxName};
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
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("demo"));
        assert!(rendered.contains("live"));
        assert!(rendered.contains("2"));
        assert!(rendered.contains("1h"));
        assert!(rendered.contains("5m"));
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
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("active"));
    }
}
