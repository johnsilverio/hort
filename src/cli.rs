//! CLI surface: the clap v4 derive definitions for the subcommands hort exposes,
//! their dispatch, and the pure `ls`, `prune`, `doctor` and warning renderers.
//!
//! Only subcommands that work end to end ship here: `up`, `attach`, `run`, `ls`,
//! `down`, `prune`, `config` and `doctor`. A command the binary names is one the
//! binary can run.
//!
//! A command that opened a session (`up` without `-d`, `attach`, `run`) leaves
//! with the status that session exited with, which is what lets a script tell
//! what ran inside a sandbox from what hort itself did. That collides with hort's
//! own exit codes, the same trade `ssh` makes.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::adapters::cache::FileCacheProvider;
use crate::adapters::clock::SystemClock;
use crate::adapters::config::{ConfigResolver, GLOBAL_FILE, find_project_dir};
use crate::adapters::confirm::StdinConfirmer;
use crate::adapters::environment::HostEnvironmentProbe;
use crate::adapters::liveness::ProcLivenessProbe;
use crate::adapters::lock::FlockSandboxLock;
use crate::adapters::metadata::FileMetadataStore;
use crate::adapters::notify::FileNotifyProvider;
use crate::adapters::pasta::PastaNetworkProvider;
use crate::adapters::prompt::DialoguerPrompter;
use crate::adapters::propose::DialoguerProposer;
use crate::adapters::runtime::LibcontainerRuntime;
use crate::adapters::terminal::HostTerminal;
use crate::adapters::worktree::GitWorktreeProvider;
use crate::commands::attach::AttachCommand;
use crate::commands::config::ConfigCommand;
use crate::commands::doctor::{ConfigurationReport, DoctorCommand, DoctorReport};
use crate::commands::down::DownCommand;
use crate::commands::ls::{LsCommand, LsEntry, WorkdirGit};
use crate::commands::prune::{PruneCommand, PruneReport};
use crate::commands::up::UpCommand;
use crate::domain::config::{GitMode, ResolvedConfig};
use crate::domain::error::HortError;
use crate::domain::idle::IdleState;
use crate::domain::model::{BranchName, Capabilities, SandboxName, Warning};
use crate::domain::onboarding::onboarding_is_due;
use crate::domain::preconditions::hard_preconditions_are_met;
use crate::domain::prune::SkipReason;
use crate::domain::reconcile::SandboxState;
use crate::ports::{Session, SessionTerminal};

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
        /// How the sandbox gets git into `/workdir`, overriding what the
        /// configuration declares: `worktree` or `clone`.
        #[arg(long, value_parser = parse_git_mode)]
        git: Option<GitMode>,
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
    /// Run one command in a running sandbox with no terminal.
    Run {
        /// The sandbox to run the command in.
        name: String,
        /// The command and its arguments, taken verbatim after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List every sandbox with its reconciled state.
    Ls,
    /// Tear a sandbox down in the mandatory order.
    Down {
        /// The sandbox to tear down.
        name: String,
        /// Skip the confirmations for open sessions and unreturned work.
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
    /// Ask what this host can do and write the global configuration.
    Config {
        /// Overwrite a configuration already on disk without asking.
        #[arg(short, long)]
        force: bool,
    },
    /// Report what this host can do, changing nothing.
    Doctor,
}

/// Read the value of `--git` as a git mode. Spelled here rather than derived on
/// the enum so the domain gains no dependency on the argument parser.
fn parse_git_mode(value: &str) -> Result<GitMode, String> {
    match value {
        "worktree" => Ok(GitMode::Worktree),
        "clone" => Ok(GitMode::Clone),
        other => Err(format!("'{other}' is not a git mode (expected 'worktree' or 'clone')")),
    }
}

/// The real adapters the commands run against, assembled once at startup.
pub struct RealDeps {
    lock: FlockSandboxLock,
    store: FileMetadataStore,
    probe: ProcLivenessProbe,
    worktrees: GitWorktreeProvider,
    runtime: LibcontainerRuntime,
    network: PastaNetworkProvider,
    terminal: HostTerminal,
    clock: SystemClock,
    confirmer: StdinConfirmer,
    prompts: DialoguerPrompter,
    proposer: DialoguerProposer,
    env: HostEnvironmentProbe,
    cache: FileCacheProvider,
    notify: FileNotifyProvider,
    config: ConfigResolver,
    /// The global configuration file: what onboarding writes, and whose absence
    /// is what says this host has never been set up.
    global_config_path: PathBuf,
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
    /// adapters. Nothing is created here: the state root is made by whichever
    /// command first writes under it, so a read-only run leaves a host as it
    /// found it. Both it and the project directory are resolved to their real
    /// paths, the state root through its nearest existing ancestor because its
    /// tail may not exist yet, so a symlinked root cannot make a record's stored
    /// worktree path disagree with the path git reports.
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
        let state_root = real_path(&state_root).map_err(|error| HortError::StateIo {
            detail: format!("could not resolve {}: {error}", state_root.display()),
        })?;

        let current_dir = std::env::current_dir()
            .map_err(|error| HortError::WorkingDirFailed {
                detail: format!("could not read the current directory: {error}"),
            })?
            .canonicalize()
            .map_err(|error| HortError::WorkingDirFailed {
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
        let config_root = resolve_config_root()?;

        Ok(Self {
            lock: FlockSandboxLock::new(state_root.clone()),
            store: FileMetadataStore::new(state_root.clone()),
            probe: ProcLivenessProbe,
            worktrees: GitWorktreeProvider::new(adapters_dir.clone(), state_root.clone()),
            runtime: LibcontainerRuntime::new(runtime_root.clone()),
            network: PastaNetworkProvider::new(runtime_root.clone()),
            terminal: HostTerminal,
            clock: SystemClock,
            confirmer: StdinConfirmer,
            prompts: DialoguerPrompter,
            proposer: DialoguerProposer,
            env: HostEnvironmentProbe,
            cache: FileCacheProvider::new(state_root.clone()),
            notify: FileNotifyProvider::new(state_root.clone(), runtime_root.clone()),
            global_config_path: config_root.join(GLOBAL_FILE),
            config: ConfigResolver::new(config_root, adapters_dir, host_home.clone()),
            state_root,
            project_dir,
            current_dir,
            host_home,
        })
    }
}

/// The directory hort keeps its per-sandbox records under: `$XDG_STATE_HOME/hort`
/// when that variable is set and not empty, otherwise the XDG default of
/// `~/.local/state/hort`.
fn resolve_state_root() -> Result<PathBuf, HortError> {
    match xdg_hort_dir("XDG_STATE_HOME") {
        Some(root) => Ok(root),
        None => Ok(home_dir()?.join(".local").join("state").join("hort")),
    }
}

/// The directory hort reads its global configuration from: `$XDG_CONFIG_HOME/hort`
/// when that variable is set and not empty, otherwise the XDG default of
/// `~/.config/hort`.
fn resolve_config_root() -> Result<PathBuf, HortError> {
    match xdg_hort_dir("XDG_CONFIG_HOME") {
        Some(root) => Ok(root),
        None => Ok(home_dir()?.join(".config").join("hort")),
    }
}

/// The directory hort keeps everything a restart makes meaningless in: the
/// container states, and the files the host-side helpers of a sandbox write.
/// `$XDG_RUNTIME_DIR/hort` when that variable is set and not empty, otherwise
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

/// The real path of `path`, which need not exist yet: its nearest existing
/// ancestor canonicalized and the rest appended, creating nothing on the way.
fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(error);
            };
            Ok(real_path(parent)?.join(name))
        }
        resolved => resolved,
    }
}

/// Dispatch a parsed command to its coordinator, printing what a script reads
/// (the `ls` rows, the `prune` report) to stdout and the advisories a build
/// raised to stderr. A returned error propagates to the binary, which prints it
/// once.
pub fn run(cli: Cli, deps: &RealDeps) -> Result<u8, HortError> {
    match cli.command {
        CliCommand::Up { name, branch, git, detach } => {
            let name = SandboxName::new(&name)?;
            let branch = branch.as_deref().map(BranchName::new).transpose()?;
            // Read here rather than at assembly: configuration is what a sandbox
            // is built out of, and a project whose configuration hort cannot
            // parse still has sandboxes to list and tear down.
            let (config, config_warnings) = resolve_configuration(deps)?;
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
                &deps.notify,
                &deps.proposer,
                deps.state_root.clone(),
                deps.project_dir.clone(),
                deps.current_dir.clone(),
                deps.host_home.clone(),
                &config,
            );
            let warnings =
                command.run(name.clone(), branch, git, std::io::stdin().is_terminal())?;
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
            let (config, config_warnings) = resolve_configuration(deps)?;
            eprint!("{}", render_warnings(&config_warnings, &[]));
            open_session(deps, name, &config)
        }
        CliCommand::Run { name, command } => {
            let name = SandboxName::new(&name)?;
            let (config, config_warnings) = resolve_configuration(deps)?;
            eprint!("{}", render_warnings(&config_warnings, &[]));
            run_in_session(deps, name, command, &config)
        }
        CliCommand::Ls => {
            let command = LsCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.worktrees,
                &deps.runtime,
                &deps.clock,
                &deps.notify,
            );
            let entries = command.run()?;
            print!("{}", render_ls(&entries));
            Ok(HORT_SUCCEEDED)
        }
        CliCommand::Down { name, force } => {
            let name = SandboxName::new(&name)?;
            let command = DownCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.runtime,
                &deps.confirmer,
                &deps.runtime,
                &deps.network,
                &deps.worktrees,
                &deps.notify,
            );
            command.run(name, force, std::io::stdin().is_terminal())?;
            Ok(HORT_SUCCEEDED)
        }
        CliCommand::Prune { force, idle } => {
            let command = PruneCommand::new(
                &deps.store,
                &deps.runtime,
                &deps.worktrees,
                &deps.runtime,
                &deps.clock,
                &deps.confirmer,
                &deps.runtime,
                &deps.network,
                &deps.cache,
                &deps.notify,
                deps.state_root.clone(),
            );
            let report = command.run(idle, force, std::io::stdin().is_terminal())?;
            print!("{}", render_prune(&report));
            Ok(HORT_SUCCEEDED)
        }
        // Straight to the dialogue rather than through the configuration
        // helper: that helper opens this same dialogue for a command that needs
        // a configuration it cannot find, and this command is the dialogue, so
        // routing it there would run the questions twice on a first run.
        CliCommand::Config { force } => {
            let command = ConfigCommand::new(
                &deps.env,
                &deps.prompts,
                deps.global_config_path.clone(),
                deps.host_home.clone(),
            );
            let warnings = command.run(force, std::io::stdin().is_terminal())?;
            eprint!("{}", render_warnings(&warnings, &[]));
            Ok(HORT_SUCCEEDED)
        }
        // Resolved here and not through the configuration helper, which opens
        // the first-run dialogue: this command reads the host and writes
        // nothing, and a read-only report that stops to ask questions and leave
        // a file behind is no longer one.
        CliCommand::Doctor => {
            let resolved = deps.config.resolve();
            let report = match &resolved {
                Ok((config, _)) => DoctorCommand::new(&deps.env, config).run(),
                Err(error) => DoctorCommand::on_unreadable_configuration(&deps.env, error).run(),
            };
            print!("{}", render_doctor(&report));
            if let Ok((_, config_warnings)) = &resolved {
                eprint!("{}", render_warnings(config_warnings, &[]));
            }
            match hard_preconditions_are_met(&report.capabilities) {
                true => Ok(HORT_SUCCEEDED),
                false => Ok(HOST_CANNOT_BUILD_A_SANDBOX),
            }
        }
    }
}

/// The configuration a command runs against, opening the first-run dialogue
/// first on a host hort has never been set up on.
///
/// The dialogue hangs off the two commands that need configuration rather than
/// off the assembly every command crosses, where a plain `hort ls` would stop to
/// ask questions nobody invoked. What the dialogue writes is then resolved like
/// any other configuration, so the command the person typed goes on rather than
/// having to be typed again.
fn resolve_configuration(deps: &RealDeps) -> Result<(ResolvedConfig, Vec<Warning>), HortError> {
    let stdin_is_tty = std::io::stdin().is_terminal();
    if onboarding_is_due(deps.global_config_path.exists(), stdin_is_tty) {
        let onboarding = ConfigCommand::new(
            &deps.env,
            &deps.prompts,
            deps.global_config_path.clone(),
            deps.host_home.clone(),
        );
        // Never forced: onboarding is offered only where there is no file, so
        // there is nothing an unasked overwrite could take.
        let warnings = onboarding.run(false, stdin_is_tty)?;
        // Printed now rather than handed back with the caller's own, which reach
        // the terminal only once the command has succeeded. What the dialogue
        // has to say about the file it just wrote is most worth reading on the
        // run that then stops on the very thing it warned about.
        eprint!("{}", render_warnings(&warnings, &[]));
    }
    deps.config.resolve()
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
    let opened = session_command(deps, config).run(name, std::io::stdin().is_terminal())?;
    relay_session(deps, opened)
}

/// Run `command` in `name` with no terminal and report what it exited with, so a
/// script or an orchestrator can tell what ran inside the box from what hort
/// itself did. It is `open_session` with the caller's command instead of a login
/// shell and no pty to allocate.
fn run_in_session(
    deps: &RealDeps,
    name: SandboxName,
    command: Vec<String>,
    config: &ResolvedConfig,
) -> Result<u8, HortError> {
    let opened = session_command(deps, config).run_command(name, command)?;
    relay_session(deps, opened)
}

/// The session command wired to the real adapters, shared by the two paths that
/// open one. It grows a member whenever a session learns to carry one more thing,
/// so building it in one place is what keeps `attach` and `run` from drifting
/// apart on the next such addition.
fn session_command<'a>(deps: &'a RealDeps, config: &'a ResolvedConfig) -> AttachCommand<'a> {
    AttachCommand::new(
        &deps.store,
        &deps.probe,
        &deps.runtime,
        &deps.clock,
        &deps.env,
        &deps.network,
        config,
        std::env::var("SHELL").ok(),
        std::env::vars().collect(),
    )
}

/// Print whatever the session had to go without, then hold its terminal until it
/// ends and hand back the code the caller leaves with.
fn relay_session(deps: &RealDeps, opened: (Session, Vec<Warning>)) -> Result<u8, HortError> {
    let (session, warnings) = opened;
    eprint!("{}", render_warnings(&[], &warnings));
    Ok(session_exit_code(deps.terminal.relay(session)?))
}

const DASH: &str = "-";

/// What hort leaves with when it ran a command of its own rather than a session.
const HORT_SUCCEEDED: u8 = 0;

/// What `doctor` leaves with when this host is missing something no
/// configuration can supply, so a script can gate on the report it just printed.
const HOST_CANNOT_BUILD_A_SANDBOX: u8 = 1;

/// What a shell adds to the signal number when it reports a process that was
/// killed rather than one that returned.
const SIGNALLED_EXIT_BASE: u8 = 128;

/// Render the `ls` rows for the terminal: one line per sandbox with its name,
/// lowercase state, session count, age, idle, branch, and worktree dirty state. A
/// figure with no value renders as a dash, and a sandbox with a running session
/// renders its idle as `active`. A sandbox the kernel is running and hort has no
/// record of carries the command that collects it under its row.
pub fn render_ls(entries: &[LsEntry]) -> String {
    entries
        .iter()
        .map(|entry| format!("{}\n{}", render_line(entry), render_advice(entry)))
        .collect()
}

/// Render the `prune` report for the terminal: the sandboxes it removed, the
/// caches it collected named by the project each belonged to, and the names it
/// skipped with the reason for each. Layout is free; only the presence of the
/// names and reasons is a contract.
pub fn render_prune(report: &PruneReport) -> String {
    let removed = report.removed.iter().map(|name| format!("removed {name}\n"));
    let caches =
        report.removed_caches.iter().map(|project| format!("removed cache of {project}\n"));
    let skipped = report
        .skipped
        .iter()
        .map(|skip| format!("skipped {} ({})\n", skip.name, skip_reason_label(&skip.reason)));
    removed.chain(caches).chain(skipped).collect()
}

/// Render the `doctor` report for the terminal: one entry per capability with
/// whether this host has it, and for one it lacks the consequence and how to
/// get it. The configured rootfs is reported through the very error a build
/// would raise, so the two never come to say different things about the same
/// directory, and a configuration hort could not parse is a row of its own
/// carrying what the parser said, with no rootfs row to draw from it. Layout is
/// free; what is a contract is that every capability the probe reads is answered
/// from the host rather than listed.
pub fn render_doctor(report: &DoctorReport) -> String {
    let host: String = host_findings(&report.capabilities).iter().map(Finding::render).collect();
    let configuration = configuration_finding(&report.configuration).render();
    format!("host\n{host}\nconfiguration\n{configuration}")
}

/// One line of the report: what was asked about, what this host answered, and,
/// where the answer is a lack, what that costs and how to get it back.
struct Finding {
    subject: String,
    answer: String,
    missing: Option<String>,
}

/// Where an answer starts on its line, counted from the left margin, so the
/// answers read as a column whatever the names in front of them are.
const ANSWER_COLUMN: usize = 23;

impl Finding {
    fn render(&self) -> String {
        let subject = format!("  {}", self.subject);
        let found = format!("{subject:<ANSWER_COLUMN$}{}\n", self.answer);
        match &self.missing {
            // Under the row rather than out at the answer column: what a lack
            // costs runs to a sentence or two, and a terminal folding that at
            // column 23 leaves a paragraph nobody reads at three in the morning.
            Some(cost) => format!("{found}    {cost}\n"),
            None => found,
        }
    }
}

/// Every capability the detection reads, in the order somebody debugging works
/// down them: what stops a sandbox existing at all, then what one would be
/// missing once it does.
fn host_findings(caps: &Capabilities) -> Vec<Finding> {
    vec![
        present_or_not(
            "user namespaces",
            caps.user_ns,
            "a sandbox is a user namespace, so nothing gets built here at all. Look at the user.max_user_namespaces sysctl, and at whatever security profile your distribution ships.",
        ),
        found_on_path(
            "pasta",
            caps.pasta.as_deref(),
            "no sandbox gets built: up refuses before it starts, because pasta is what bridges one to the network. It ships in the passt package.",
        ),
        found_on_path(
            "ip",
            caps.ip.as_deref(),
            "only an egress allowlist needs it, to empty the sandbox's routing table; an open sandbox builds fine without it. It ships in iproute2.",
        ),
        delegated_controller("memory", caps.cgroup.memory, "a sandbox runs with no memory ceiling"),
        delegated_controller(
            "pids",
            caps.cgroup.pids,
            "nothing caps how many processes a sandbox forks",
        ),
        delegated_controller("cpu", caps.cgroup.cpu, "a sandbox runs with no CPU ceiling"),
        delegated_controller(
            "cpuset",
            caps.cgroup.cpuset,
            "a sandbox cannot be pinned to a set of cores",
        ),
        landlock_finding(caps.landlock_abi),
        present_or_not(
            "rootless overlayfs",
            caps.overlayfs_rootless,
            "every sandbox root is an overlay, so a build gets as far as the mount and dies there. The kernel has to offer overlay to an unprivileged user namespace.",
        ),
        found_on_path(
            "notify-send",
            caps.notify_send.as_deref(),
            "an agent announcing that it finished gets recorded and nothing reaches the screen. It ships in libnotify.",
        ),
        present_or_not(
            "git",
            caps.git,
            "no sandbox gets built, in a repository or in a marked folder alike: up refuses before it starts, because git is what tells those two apart and what prepares the worktree. Install it.",
        ),
    ]
}

/// The one row that is not a host fact: what came of reading the configuration
/// and, where it read, whether the rootfs it names could carry a sandbox.
///
/// A rootfs that could not is reported through the very error a build would
/// raise, so the two can never say different things about one directory, and
/// those strings already carry both the consequence and the way out.
fn configuration_finding(report: &ConfigurationReport) -> Finding {
    match report {
        ConfigurationReport::Unreadable(complaint) => Finding {
            subject: "config file".to_string(),
            answer: complaint.to_string(),
            missing: Some(
                "everything above is still what this host can do; nothing here speaks for this project until that file parses."
                    .to_string(),
            ),
        },
        ConfigurationReport::Read { rootfs: verdict } => Finding {
            subject: "rootfs".to_string(),
            answer: match verdict {
                Some(unusable) => unusable.to_string(),
                None => "ready".to_string(),
            },
            missing: None,
        },
    }
}

/// A capability the detection either observed or did not, with nothing to name
/// beyond that.
fn present_or_not(subject: &str, present: bool, cost: &str) -> Finding {
    Finding {
        subject: subject.to_string(),
        answer: match present {
            true => "yes".to_string(),
            false => "no".to_string(),
        },
        missing: (!present).then(|| cost.to_string()),
    }
}

/// A binary the detection went looking for on the `PATH`, answered with where it
/// landed, because a host carrying two of them is exactly the host somebody runs
/// this on.
fn found_on_path(subject: &str, found: Option<&Path>, cost: &str) -> Finding {
    Finding {
        subject: subject.to_string(),
        answer: match found {
            Some(path) => path.display().to_string(),
            None => "not on PATH".to_string(),
        },
        missing: found.is_none().then(|| cost.to_string()),
    }
}

/// One cgroup controller and what a sandbox goes without where this user was not
/// delegated it. The way to get any of them is the same systemd directive, so
/// the row composes that half itself rather than repeating it four times.
fn delegated_controller(controller: &str, delegated: bool, cost: &str) -> Finding {
    Finding {
        subject: format!("cgroup {controller}"),
        answer: match delegated {
            true => "delegated".to_string(),
            false => "not delegated".to_string(),
        },
        missing: (!delegated).then(|| {
            format!("{cost}. Add {controller} to Delegate= in a systemd drop-in for user@.service.")
        }),
    }
}

/// The Landlock row, which answers with the ABI rather than with a yes: which
/// version the kernel offers is what decides whether the egress half of the
/// restriction can be applied at all.
fn landlock_finding(abi: Option<u8>) -> Finding {
    Finding {
        subject: "landlock".to_string(),
        answer: match abi {
            Some(abi) => format!("ABI {abi}"),
            None => "unavailable".to_string(),
        },
        missing: abi.is_none().then(|| {
            "an allowlisted sandbox loses the kernel restriction on which ports a session may dial; the routeless namespace, the proxy and the absent resolver still hold. ABI 4 or later carries that port half."
                .to_string()
        }),
    }
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
    let mut columns = vec![
        entry.name.as_str().to_string(),
        state_label(entry.state).to_string(),
        render_sessions(entry.sessions),
        render_duration(entry.age),
        render_idle(entry.idle.as_ref()),
    ];
    columns.extend(entry.git.map(render_git));
    columns.push(render_branch(entry.branch.as_ref()));
    columns.push(render_dirty(entry.dirty));
    columns.join("  ")
}

/// How the box was built and, for a clone, whether it holds commits its own
/// project lacks, in the words `prune` skips such a box with, since it is the
/// same question. A clone hort could not read says so rather than printing like
/// one holding nothing, which is the row somebody collects. A row with no mode
/// to report (no record, no git, or a `/workdir` gone) has no such column at
/// all, which keeps branch and dirty the last two columns of every row.
fn render_git(git: WorkdirGit) -> String {
    match git {
        WorkdirGit::Worktree => "worktree".to_string(),
        WorkdirGit::Clone { unreturned: Some(false) } => "clone".to_string(),
        WorkdirGit::Clone { unreturned: Some(true) } => {
            format!("clone, {}", skip_reason_label(&SkipReason::UnreturnedWork))
        }
        WorkdirGit::Clone { unreturned: None } => "clone, work unknown".to_string(),
    }
}

/// Each reason in the words that send the reader to the right place. A cache skip
/// that read like a worktree skip would have them hunting for uncommitted changes
/// in a directory that holds none, which is why no two of the four questions share
/// a label. The last three matter most: they are the only reasons `--force` does
/// not clear, so a reader told the same thing it tells them about a live project
/// would pass the flag, watch nothing happen, and find no line explaining why.
/// Their words are apart from each other for the same reason, since downing a
/// named box, going to look at `ls`, and finding out who is inside a box hort
/// could not read are different things to do next.
fn skip_reason_label(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::Dirty => "dirty",
        SkipReason::UnreturnedWork => "work only in the box",
        SkipReason::Unknown => "unknown",
        SkipReason::LiveProject => "project on disk",
        SkipReason::UnknownProject => "project unreadable",
        SkipReason::LiveSandbox => "sandbox running",
        SkipReason::UnknownSandbox => "sandbox unaccounted for",
        SkipReason::UnknownIdle => "idle unknown",
    }
}

fn render_sessions(sessions: Option<usize>) -> String {
    match sessions {
        Some(count) => count.to_string(),
        None => DASH.to_string(),
    }
}

fn render_dirty(dirty: Option<bool>) -> String {
    match dirty {
        Some(true) => "dirty".to_string(),
        Some(false) => "clean".to_string(),
        None => DASH.to_string(),
    }
}

/// What to do about a row, for the one state where naming it is not enough. A
/// lost record is a sandbox the kernel is running that hort has no memory of, so
/// the reader is holding a name that `up`, `attach` and `prune` all answer for a
/// different situation. The row therefore hands over the ready command rather
/// than the fact that one exists, and only here: the same sentence under a
/// healthy box would be hort telling somebody to destroy a sandbox that is
/// working exactly as it should.
fn render_advice(entry: &LsEntry) -> String {
    match entry.state {
        SandboxState::LostRecord => format!(
            "    running with no record on disk; run 'hort down {}' to stop its container and host-side helpers\n",
            entry.name.as_str()
        ),
        SandboxState::Live | SandboxState::Orphaned | SandboxState::Inconsistent => String::new(),
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

    use std::fs;
    use std::time::Duration;

    use crate::commands::ls::WorkdirGit;
    use crate::domain::idle::IdleState;
    use crate::domain::model::{BranchName, Capabilities, CgroupCaps, SandboxName, Warning};
    use crate::domain::prune::{PruneSkip, SkipReason};
    use crate::domain::reconcile::SandboxState;

    #[test]
    fn render_ls_includes_each_required_column_for_entry() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: Some(2),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
            git: Some(WorkdirGit::Worktree),
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
            sessions: Some(0),
            age: None,
            idle: None,
            branch: None,
            dirty: None,
            git: None,
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("lost-record"));
        assert!(rendered.contains("-"));
    }

    #[test]
    fn render_ls_teaches_the_collection_command_only_on_the_lost_record_row() {
        let live = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: Some(1),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Active),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
            git: Some(WorkdirGit::Worktree),
        };
        let lost = LsEntry {
            name: SandboxName::new("ghost").unwrap(),
            state: SandboxState::LostRecord,
            sessions: Some(0),
            age: None,
            idle: None,
            branch: None,
            dirty: None,
            git: None,
        };

        let rendered = render_ls(&[live, lost]);

        // A lost record is a box the kernel is running that every command used to
        // deny the existence of, so the listing that names the state hands over
        // the command that collects it rather than teaching that one exists. The
        // wording around it is free; the ready command is the guarantee.
        assert!(rendered.contains("hort down ghost"));
        // And only there: the same advice against a healthy box would be hort
        // telling a reader to destroy a sandbox somebody is working in.
        assert!(!rendered.contains("hort down demo"));
    }

    #[test]
    fn render_ls_renders_unknown_sessions_as_a_dash() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: None,
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
            git: Some(WorkdirGit::Worktree),
        };

        let rendered = render_ls(&[entry]);

        // Every other column of this row is deliberately known, so the only
        // dash the line can hold is the session count. Printed as a zero it
        // reads as a box nobody is in, which is a claim hort did not make.
        assert!(rendered.contains("-"));
    }

    #[test]
    fn render_prune_tells_an_unknown_idle_from_an_unreadable_worktree() {
        let worktree = PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }],
        };
        let idle = PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::UnknownIdle }],
        };

        // One sends the reader to a worktree to look for uncommitted work that
        // --force would spend; the other says hort could not tell how long the
        // box has been idle, which --force cannot lift at all. The same word for
        // both is a user passing the flag and watching nothing happen.
        assert_ne!(render_prune(&worktree), render_prune(&idle));
    }

    #[test]
    fn render_ls_renders_running_sessions_as_active() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: Some(1),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Active),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
            git: Some(WorkdirGit::Worktree),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("active"));
    }

    #[test]
    fn render_prune_lists_removed_and_skipped() {
        let report = PruneReport {
            removed: vec!["demo".to_string()],
            removed_caches: Vec::new(),
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
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }],
        };

        let rendered = render_prune(&report);

        // This line is what the user reads before deciding whether to pass
        // --force, and "dirty" would send them looking for uncommitted changes
        // in a worktree whose repository is gone.
        assert!(rendered.contains("unknown"));
    }

    #[test]
    fn render_prune_reports_work_only_the_box_holds_as_its_own_reason() {
        let report = PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip {
                name: "demo".to_string(),
                reason: SkipReason::UnreturnedWork,
            }],
        };

        let rendered = render_prune(&report);

        // Same argument as the unknown arm above: this line is read before
        // deciding whether to force, and "dirty" would send the user looking for
        // uncommitted changes in a box whose work is committed, finding none,
        // and forcing away the commits themselves.
        assert!(rendered.contains("work only in the box"));
    }

    #[test]
    fn render_prune_names_the_cache_it_removed() {
        let report = PruneReport {
            removed: Vec::new(),
            removed_caches: vec!["/home/tester/projects/gone".to_string()],
            skipped: Vec::new(),
        };

        let rendered = render_prune(&report);

        // A collected cache that nothing prints is a directory the user watches
        // disappear with no line of output saying it did.
        assert!(rendered.contains("/home/tester/projects/gone"));
    }

    #[test]
    fn render_prune_tells_an_unreadable_project_from_an_unreadable_worktree() {
        let worktree = PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip { name: "demo".to_string(), reason: SkipReason::Unknown }],
        };
        let project = PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip {
                name: "demo".to_string(),
                reason: SkipReason::UnknownProject,
            }],
        };

        // The two reasons exist as separate values for exactly one purpose: one
        // sends the reader to look at a worktree and the other at a project
        // folder. Rendered with the same word they are one value again, and the
        // reader goes to the wrong place.
        assert_ne!(render_prune(&worktree), render_prune(&project));
    }

    #[test]
    fn render_prune_tells_a_held_cache_from_a_live_project() {
        let held = cache_skip_report(SkipReason::LiveSandbox);
        let live_project = cache_skip_report(SkipReason::LiveProject);

        // Told the project is still on disk, a user passes --force and gets what
        // they asked for. Told the same thing about a cache a running box is
        // standing on, they pass --force, nothing happens, and no line on the
        // screen explains why.
        assert_ne!(render_prune(&held), render_prune(&live_project));
    }

    #[test]
    fn render_prune_tells_an_unplaceable_sandbox_from_an_unreadable_project() {
        let unplaceable = cache_skip_report(SkipReason::UnknownSandbox);
        let unreadable = cache_skip_report(SkipReason::UnknownProject);

        // Same trap, the other half of it: --force releases a project hort could
        // not read and never releases a sandbox it could not place.
        assert_ne!(render_prune(&unplaceable), render_prune(&unreadable));
    }

    #[test]
    fn render_prune_tells_a_held_cache_from_one_it_cannot_place() {
        let held = cache_skip_report(SkipReason::LiveSandbox);
        let unplaceable = cache_skip_report(SkipReason::UnknownSandbox);

        // Neither is cleared by --force, so what is left to tell the user is
        // what to do, and the two answers are different: down that box and run
        // again, or go look at what `ls` says is running.
        assert_ne!(render_prune(&held), render_prune(&unplaceable));
    }

    /// A report of one skipped cache, named by its project as every line a
    /// person reads is.
    fn cache_skip_report(reason: SkipReason) -> PruneReport {
        PruneReport {
            removed: Vec::new(),
            removed_caches: Vec::new(),
            skipped: vec![PruneSkip { name: "/home/tester/projects/hort".to_string(), reason }],
        }
    }

    fn a_capable_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: true },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            git: true,
        }
    }

    fn reported(capabilities: Capabilities) -> DoctorReport {
        DoctorReport { capabilities, configuration: ConfigurationReport::Read { rootfs: None } }
    }

    #[test]
    fn render_doctor_tells_a_host_apart_by_every_capability_it_reports() {
        let capable = render_doctor(&reported(a_capable_host()));
        let each_one_missing = [
            ("user namespaces", Capabilities { user_ns: false, ..a_capable_host() }),
            ("pasta", Capabilities { pasta: None, ..a_capable_host() }),
            ("ip", Capabilities { ip: None, ..a_capable_host() }),
            (
                "a delegated memory controller",
                Capabilities {
                    cgroup: CgroupCaps { memory: false, ..a_capable_host().cgroup },
                    ..a_capable_host()
                },
            ),
            (
                "a delegated pids controller",
                Capabilities {
                    cgroup: CgroupCaps { pids: false, ..a_capable_host().cgroup },
                    ..a_capable_host()
                },
            ),
            (
                "a delegated cpu controller",
                Capabilities {
                    cgroup: CgroupCaps { cpu: false, ..a_capable_host().cgroup },
                    ..a_capable_host()
                },
            ),
            (
                "a delegated cpuset controller",
                Capabilities {
                    cgroup: CgroupCaps { cpuset: false, ..a_capable_host().cgroup },
                    ..a_capable_host()
                },
            ),
            ("Landlock", Capabilities { landlock_abi: None, ..a_capable_host() }),
            ("rootless overlayfs", Capabilities { overlayfs_rootless: false, ..a_capable_host() }),
            ("notify-send", Capabilities { notify_send: None, ..a_capable_host() }),
            ("git", Capabilities { git: false, ..a_capable_host() }),
        ];

        // Asked as a difference and never as a name in the text: a report that
        // prints a fixed list of labels contains every name a caller could look
        // for, so a search finds them all on a report that never read anything.
        // What separates the two is that flipping a capability changes what
        // comes out, and it has to hold for each of them, since a report is
        // useless for the one capability it happens to be blind to.
        for (capability, host) in each_one_missing {
            assert_ne!(
                render_doctor(&reported(host)),
                capable,
                "a host without {capability} reads exactly like one that has it"
            );
        }
    }

    #[test]
    fn render_doctor_still_answers_for_the_host_when_the_configuration_is_broken() {
        let what_the_parser_said = "/here/.hort.json: expected value at line 1 column 14";
        let unreadable = |host: Capabilities| {
            render_doctor(&DoctorReport {
                capabilities: host,
                configuration: ConfigurationReport::Unreadable(HortError::InvalidConfig {
                    detail: what_the_parser_said.to_string(),
                }),
            })
        };

        let capable = unreadable(a_capable_host());

        // The parser's own words and not a sentence of hort's own, because the
        // line and column are the whole of what sends somebody to the right
        // place in the file.
        assert!(capable.contains(what_the_parser_said), "{capable}");
        // The other half of the same guarantee, and the half a witness looking
        // only for the complaint would miss: a report that prints that one row
        // and stops reads the same on every host, and a host whose
        // configuration is broken is exactly the one somebody is running this
        // on to find out what else is wrong.
        assert_ne!(
            capable,
            unreadable(Capabilities { pasta: None, ..a_capable_host() }),
            "a configuration hort could not read took the host report away with it"
        );
    }

    #[test]
    fn render_doctor_names_where_pasta_was_found() {
        let found_at = "/opt/hort/bin/pasta";
        let host = Capabilities { pasta: Some(PathBuf::from(found_at)), ..a_capable_host() };

        let rendered = render_doctor(&reported(host));

        // A host with two of them on the PATH is exactly the host somebody runs
        // doctor on, and "present" alone does not say which one hort would run.
        assert!(rendered.contains(found_at), "{rendered}");
    }

    #[test]
    fn render_doctor_carries_the_rootfs_error_the_host_would_raise() {
        let report = DoctorReport {
            capabilities: a_capable_host(),
            configuration: ConfigurationReport::Read {
                rootfs: Some(HortError::RootfsMissing { path: "/opt/hort/rootfs".to_string() }),
            },
        };

        let rendered = render_doctor(&report);

        // Verbatim, because this line is the one place the report is obliged to
        // say the consequence and the way out, and it is already a canonical
        // string a build would print for the same directory.
        assert!(
            rendered.contains(
                "rootfs directory '/opt/hort/rootfs' does not exist — prepare it first with podman export, debootstrap or umoci unpack"
            ),
            "{rendered}"
        );
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
            sessions: Some(0),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(true),
            git: Some(WorkdirGit::Worktree),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("dirty"));
    }

    fn clone_entry(unreturned: Option<bool>) -> LsEntry {
        LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Orphaned,
            sessions: Some(0),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: None,
            git: Some(WorkdirGit::Clone { unreturned }),
        }
    }

    #[test]
    fn render_ls_names_a_clone_by_its_mode() {
        let rendered = render_ls(&[clone_entry(Some(false))]);

        assert!(rendered.contains("clone"));
    }

    #[test]
    fn render_ls_names_a_worktree_by_its_mode() {
        let entry = LsEntry {
            name: SandboxName::new("demo").unwrap(),
            state: SandboxState::Live,
            sessions: Some(0),
            age: Some(Duration::from_secs(3600)),
            idle: Some(IdleState::Idle(Duration::from_secs(300))),
            branch: Some(BranchName::new("demo").unwrap()),
            dirty: Some(false),
            git: Some(WorkdirGit::Worktree),
        };

        let rendered = render_ls(&[entry]);

        assert!(rendered.contains("worktree"));
    }

    #[test]
    fn render_ls_says_a_clone_holds_work_only_in_the_box() {
        let holding = render_ls(&[clone_entry(Some(true))]);
        let returned = render_ls(&[clone_entry(Some(false))]);

        // The same words `prune` skips such a box with, because it is the same
        // question: a reader who sees them in both places knows it is one fact.
        assert!(holding.contains("work only in the box"));
        assert!(!returned.contains("work only in the box"));
    }

    #[test]
    fn render_ls_tells_an_unread_clone_from_one_holding_nothing() {
        let unread = render_ls(&[clone_entry(None)]);
        let returned = render_ls(&[clone_entry(Some(false))]);

        // After a reboot this row is how somebody decides whether a box is safe
        // to collect, and an unread clone printed like an empty one is a yes
        // nobody checked.
        assert_ne!(unread, returned);
    }

    #[test]
    fn state_root_is_the_real_path_when_the_state_home_is_a_symlink() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        let link = base.join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // git lists a worktree by its real path, and up decides whether to
        // resume a half-built sandbox by comparing that listing with the path
        // it derives from the state root, so the two have to be the same text.
        let state_root = link.join("hort");

        let resolved = real_path(&state_root).unwrap();

        assert_eq!(resolved, real.join("hort"));
    }
}
