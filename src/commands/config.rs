//! `config`: the first-run dialogue. Detect what this host can do, ask the four
//! things detection cannot answer, hand both to the pure generator, and write
//! the document it renders.
//!
//! The questions reach the person through a port, so the whole shape of the
//! dialogue is testable with no terminal anywhere near it. The file write is a
//! plain effect this command performs itself: the text is already decided by
//! the time it gets here, and nothing in hort ever reads that file back except
//! the configuration resolver.

use std::fs;
use std::path::PathBuf;

use crate::domain::config::expand_home;
use crate::domain::error::HortError;
use crate::domain::model::{AgentChoice, OnboardingAnswers, Warning};
use crate::domain::onboarding::generate_config;
use crate::domain::preconditions::rootfs_precondition_error;
use crate::ports::{EnvironmentProbe, Prompter};

/// The dotfiles onboarding offers to carry into every sandbox read-only, in the
/// shorthand they land in the configuration as.
///
/// A floor rather than a closed set: another entry may be added, while dropping
/// one takes an offer away from everybody who onboards after it and is a change
/// of what the command promises.
const DOTFILE_CANDIDATES: [&str; 4] =
    ["~/.config/nvim", "~/.config/fish", "~/.tmux.conf", "~/.gitconfig"];

/// The agents onboarding knows how to go looking for.
///
/// Where a tool keeps its credentials, and whether it can say when it finished,
/// is something hort detects about a host. It is held here and never crosses
/// into the generator, which decides the document without ever learning which
/// agent is which.
const AGENT_CANDIDATES: [AgentCandidate; 1] = [AgentCandidate {
    name: "Claude Code",
    command: "claude --dangerously-skip-permissions",
    credentials: "~/.claude",
    stop_hook: true,
}];

/// One agent the dialogue can offer, and what it would write for it.
struct AgentCandidate {
    /// What the question calls it.
    name: &'static str,
    /// The command the entry records, verbatim.
    command: &'static str,
    /// The host path holding its credentials, whose presence is what decides
    /// whether the agent is offered at all.
    credentials: &'static str,
    /// Whether this agent can announce that it finished.
    stop_hook: bool,
}

/// Coordinates the first-run dialogue: what the host has, what the person wants,
/// and the global configuration file the two of them produce.
pub struct ConfigCommand<'a> {
    env: &'a dyn EnvironmentProbe,
    prompts: &'a dyn Prompter,
    config_path: PathBuf,
    host_home: PathBuf,
}

impl<'a> ConfigCommand<'a> {
    pub fn new(
        env: &'a dyn EnvironmentProbe,
        prompts: &'a dyn Prompter,
        config_path: PathBuf,
        host_home: PathBuf,
    ) -> Self {
        Self { env, prompts, config_path, host_home }
    }
}

impl ConfigCommand<'_> {
    /// Ask, decide, and write the global configuration, handing back the
    /// advisories the terminal prints now.
    ///
    /// `force` decides only whether a configuration already on disk is
    /// overwritten unasked. It never stands in for the terminal: this command
    /// is the dialogue, so with nowhere to ask there is nothing left of it to
    /// run.
    pub fn run(&self, force: bool, stdin_is_tty: bool) -> Result<Vec<Warning>, HortError> {
        if !stdin_is_tty {
            return Err(HortError::ConfigNeedsTerminal);
        }

        if !force && !self.may_overwrite()? {
            return Ok(Vec::new());
        }

        let (rootfs, mut warnings) = self.ask_rootfs()?;
        let answers = OnboardingAnswers {
            rootfs,
            dotfiles: self.ask_dotfiles()?,
            agents: self.ask_agents()?,
            notifications: self.prompts.confirm(
                "raise a desktop notification when an agent announces that it finished?",
            )?,
        };

        let (document, generated) = generate_config(&self.env.detect(), &answers);
        warnings.extend(generated);
        self.write(&document.render())?;
        Ok(warnings)
    }

    /// Whether the file may be written: a path holding nothing always may, and
    /// one already holding a configuration only if the person says so.
    fn may_overwrite(&self) -> Result<bool, HortError> {
        if !self.config_path.exists() {
            return Ok(true);
        }
        let question = format!("{} already exists; overwrite it?", self.config_path.display());
        self.prompts.confirm(&question)
    }

    /// The prepared rootfs directory the person named, plus what this host says
    /// about it.
    ///
    /// A directory that cannot build a sandbox is a warning and never a
    /// refusal. The file is written naming it anyway, so what the person goes
    /// and fixes is the directory rather than the configuration, and the
    /// warning is already the message the next `up` would print.
    fn ask_rootfs(&self) -> Result<(Option<String>, Vec<Warning>), HortError> {
        if !self.prompts.confirm("do you have a prepared rootfs directory?")? {
            return Ok((None, Vec::new()));
        }

        let rootfs = self.prompts.ask("path to the prepared rootfs directory")?;
        let facts = self.env.inspect_rootfs(&self.on_the_host(&rootfs), None);
        let mut warnings = Vec::new();
        if let Some(unusable) = rootfs_precondition_error(Some(&facts)) {
            warnings.push(Warning::new(unusable.to_string()));
        }
        Ok((Some(rootfs), warnings))
    }

    /// The dotfiles the person picked, offered from the candidates this host
    /// actually has.
    fn ask_dotfiles(&self) -> Result<Vec<String>, HortError> {
        let present = self.present_among(&DOTFILE_CANDIDATES);
        self.prompts.choose("which of these should every sandbox mount read-only?", &present)
    }

    /// The agent entries the person accepted, offered only for the agents whose
    /// credentials are on this host: with nothing to mount, the entry would
    /// promise an agent that still lands on a login prompt.
    fn ask_agents(&self) -> Result<Vec<AgentChoice>, HortError> {
        let credentials: Vec<PathBuf> =
            AGENT_CANDIDATES.iter().map(|agent| self.on_the_host(agent.credentials)).collect();
        let found = self.env.inspect_mount_sources(&credentials);

        let mut chosen = Vec::new();
        for (agent, credentials) in AGENT_CANDIDATES.iter().zip(found) {
            if !credentials.exists {
                continue;
            }
            let question = format!(
                "add {} to the agents list, mounting {} read-only?",
                agent.name, agent.credentials
            );
            if self.prompts.confirm(&question)? {
                chosen.push(AgentChoice {
                    command: agent.command.to_string(),
                    auth_read_only: vec![agent.credentials.to_string()],
                    stop_hook: agent.stop_hook,
                });
            }
        }
        Ok(chosen)
    }

    /// The candidates this host has, in the order asked about and still written
    /// the way they land in the configuration.
    fn present_among(&self, candidates: &[&str]) -> Vec<String> {
        let paths: Vec<PathBuf> =
            candidates.iter().map(|candidate| self.on_the_host(candidate)).collect();
        self.env
            .inspect_mount_sources(&paths)
            .iter()
            .zip(candidates)
            .filter(|(found, _)| found.exists)
            .map(|(_, candidate)| (*candidate).to_string())
            .collect()
    }

    /// A candidate as the host sees it. The shorthand is what a reader of the
    /// file expects to find, and it is not what a stat resolves.
    fn on_the_host(&self, path: &str) -> PathBuf {
        PathBuf::from(expand_home(path, &self.host_home))
    }

    /// Put the rendered document on disk, naming the step that failed.
    ///
    /// The two steps are reported apart because a directory that could not be
    /// made is a different place to go and look than a file that could not be
    /// written, and the file never existed in that case.
    fn write(&self, document: &str) -> Result<(), HortError> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent).map_err(|error| HortError::ConfigWriteFailed {
                detail: format!("could not create {}: {error}", parent.display()),
            })?;
        }
        fs::write(&self.config_path, document).map_err(|error| HortError::ConfigWriteFailed {
            detail: format!("could not write {}: {error}", self.config_path.display()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::domain::config;
    use crate::domain::model::{Capabilities, CgroupCaps};
    use crate::fakes::{FakeCapabilities, ScriptedPrompter};

    const A_PREPARED_ROOTFS: &str = "~/.local/share/hort/rootfs/devbox";
    const A_ROOTFS_THE_HOST_DOES_NOT_HAVE: &str = "/nowhere/prepared-rootfs";

    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            git: true,
        }
    }

    fn config_path(home: &Path) -> PathBuf {
        home.join(".config").join("hort").join("config.json")
    }

    #[test]
    fn config_writes_the_document_the_answers_produce() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        command.run(false, true).unwrap();

        let written = fs::read_to_string(&path).expect("onboarding writes the global config");
        assert_eq!(
            config::parse(&written)
                .expect("and hort can read back what it wrote")
                .rootfs
                .as_deref(),
            Some(A_PREPARED_ROOTFS),
            "the one field detection cannot answer is the one the person typed: {written}"
        );
    }

    #[test]
    fn config_warns_about_a_rootfs_the_host_cannot_use() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        let env = FakeCapabilities::new(ready_host()).with_missing_rootfs();
        let prompts = ScriptedPrompter::accepting().answering(A_ROOTFS_THE_HOST_DOES_NOT_HAVE);
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        let warnings = command.run(false, true).unwrap();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.to_string().contains(A_ROOTFS_THE_HOST_DOES_NOT_HAVE)),
            "the path that cannot build a sandbox is named while the person is still there to fix it: {warnings:?}"
        );
        assert_eq!(
            config::parse(&fs::read_to_string(&path).unwrap())
                .expect("hort can read back what it wrote")
                .rootfs
                .as_deref(),
            Some(A_ROOTFS_THE_HOST_DOES_NOT_HAVE),
            "and it is written active rather than refused, so a later up names that same path"
        );
    }

    #[test]
    fn config_offers_only_the_dotfile_candidates_the_host_has() {
        let home = TempDir::new().unwrap();
        let absent = home.path().join(".config").join("fish");
        let env =
            FakeCapabilities::new(ready_host()).with_missing_mount_source(absent.to_str().unwrap());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command =
            ConfigCommand::new(&env, &prompts, config_path(home.path()), home.path().to_path_buf());

        command.run(false, true).unwrap();

        let offered = prompts.offers();
        assert!(
            offered.contains(&"~/.config/nvim".to_string()),
            "a dotfile this host has is put in front of the person: {offered:?}"
        );
        assert!(
            !offered.contains(&"~/.config/fish".to_string()),
            "and one it does not have is not offered at all: {offered:?}"
        );
    }

    #[test]
    fn config_offers_an_agent_entry_when_its_credential_directory_is_there() {
        let with_credentials = TempDir::new().unwrap();
        let without_credentials = TempDir::new().unwrap();
        let env_with = FakeCapabilities::new(ready_host());
        let env_without = FakeCapabilities::new(ready_host()).with_missing_mount_sources();
        let prompts_with = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let prompts_without = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let on_a_host_that_has_them = ConfigCommand::new(
            &env_with,
            &prompts_with,
            config_path(with_credentials.path()),
            with_credentials.path().to_path_buf(),
        );
        let on_a_host_that_does_not = ConfigCommand::new(
            &env_without,
            &prompts_without,
            config_path(without_credentials.path()),
            without_credentials.path().to_path_buf(),
        );

        on_a_host_that_has_them.run(false, true).unwrap();
        on_a_host_that_does_not.run(false, true).unwrap();

        let written_with =
            config::parse(&fs::read_to_string(config_path(with_credentials.path())).unwrap())
                .expect("hort can read back what it wrote");
        let written_without =
            config::parse(&fs::read_to_string(config_path(without_credentials.path())).unwrap())
                .expect("hort can read back what it wrote");
        assert_eq!(
            written_with.agents.first().map(|agent| agent.auth.read_only.clone()),
            Some(vec!["~/.claude".to_string()]),
            "the entry names the credentials the host actually keeps"
        );
        assert!(
            written_without.agents.is_empty(),
            "and a host with no credentials for it is never offered the entry"
        );
    }

    #[test]
    fn config_declares_the_completion_hook_of_the_agent_it_adds() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        command.run(false, true).unwrap();

        let written = config::parse(&fs::read_to_string(&path).unwrap())
            .expect("hort can read back what it wrote");
        assert_eq!(
            written
                .agents
                .first()
                .and_then(|agent| agent.notify.as_ref())
                .map(|notify| notify.stop_hook),
            Some(true),
            "the entry declares the hook, which is the only thing that arms the notification chain"
        );
    }

    #[test]
    fn config_prompts_before_overwriting_an_existing_config() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "rootfs": "/already/configured" }"#).unwrap();
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::declining();
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        command.run(false, true).unwrap();

        assert!(
            !prompts.questions().is_empty(),
            "the person is asked rather than told, so refusing is something they can do"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{ "rootfs": "/already/configured" }"#,
            "and the answer is honoured: what was there is still there"
        );
    }

    #[test]
    fn config_overwrites_without_prompting_under_force() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "rootfs": "/already/configured" }"#).unwrap();
        let env = FakeCapabilities::new(ready_host());
        // Declining, so a run that still asked about the overwrite would stop here.
        let prompts = ScriptedPrompter::declining();
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        command.run(true, true).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("/already/configured"),
            "the file that was there is gone, without the answer deciding it: {written}"
        );
    }

    #[test]
    fn config_refuses_without_a_terminal() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command = ConfigCommand::new(&env, &prompts, path.clone(), home.path().to_path_buf());

        // Forced, because the flag decides whether an existing file is
        // overwritten and never whether the questions can go unasked.
        let result = command.run(true, false);

        assert_eq!(result, Err(HortError::ConfigNeedsTerminal));
        assert!(prompts.questions().is_empty(), "nothing was asked");
        assert!(!path.exists(), "and nothing was written");
    }

    #[test]
    fn config_reports_no_host_precondition_of_its_own() {
        let where_it_cannot = TempDir::new().unwrap();
        let where_it_can = TempDir::new().unwrap();
        let cannot_build_a_sandbox =
            Capabilities { user_ns: false, pasta: None, ip: None, ..ready_host() };
        // Absolute on purpose: a `~` answer expands into each run's own home, so
        // the advisory naming it would differ for a reason that is not the host.
        let answer = A_ROOTFS_THE_HOST_DOES_NOT_HAVE;
        let env_without = FakeCapabilities::new(cannot_build_a_sandbox).with_missing_rootfs();
        let env_with = FakeCapabilities::new(ready_host()).with_missing_rootfs();
        let prompts_without = ScriptedPrompter::accepting().answering(answer);
        let prompts_with = ScriptedPrompter::accepting().answering(answer);
        let on_a_host_that_cannot_build_one = ConfigCommand::new(
            &env_without,
            &prompts_without,
            config_path(where_it_cannot.path()),
            where_it_cannot.path().to_path_buf(),
        );
        // The capable host is the control: demanding no advisory at all would
        // also forbid the ones onboarding exists to raise.
        let on_a_host_that_can = ConfigCommand::new(
            &env_with,
            &prompts_with,
            config_path(where_it_can.path()),
            where_it_can.path().to_path_buf(),
        );

        let said_where_it_cannot = on_a_host_that_cannot_build_one.run(false, true).unwrap();
        let said_where_it_can = on_a_host_that_can.run(false, true).unwrap();

        assert_eq!(
            said_where_it_cannot, said_where_it_can,
            "onboarding reads the host and builds nothing, so what building a sandbox would need of this one is never its complaint: {said_where_it_cannot:?}"
        );
    }

    #[test]
    fn config_write_failure_names_the_configuration_and_not_the_state_root() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        // A directory standing where the file belongs, so the rendered document
        // has nowhere to land.
        fs::create_dir_all(&path).unwrap();
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command = ConfigCommand::new(&env, &prompts, path, home.path().to_path_buf());

        // Forced, because something already sits at that path and the overwrite
        // question is not what this measures.
        let refusal = command.run(true, true).expect_err("nothing can be written there");

        let message = refusal.to_string();
        assert!(
            message.contains("configuration"),
            "whoever reads this is on their first run of hort, and the configuration is what failed: {message}"
        );
        assert!(
            !message.contains("state directory"),
            "and the state directory is neither where it failed nor anything the person can go and fix: {message}"
        );
    }

    #[test]
    fn config_write_failure_names_the_directory_it_could_not_create() {
        let home = TempDir::new().unwrap();
        let path = config_path(home.path());
        let directory = path.parent().unwrap().to_path_buf();
        // A file standing where the config root belongs, so the directory the
        // file needs cannot be made and the write is never reached.
        fs::write(home.path().join(".config"), "not a directory").unwrap();
        let env = FakeCapabilities::new(ready_host());
        let prompts = ScriptedPrompter::accepting().answering(A_PREPARED_ROOTFS);
        let command = ConfigCommand::new(&env, &prompts, path, home.path().to_path_buf());

        let refusal = command.run(false, true).expect_err("that directory cannot be made");

        // The directory is a prefix of the file path, so naming the file alone
        // satisfies the first assertion; the second is what tells the two
        // failures apart.
        let message = refusal.to_string();
        assert!(
            message.contains(&directory.display().to_string()),
            "the directory that could not be made is the one to go and look at: {message}"
        );
        assert!(
            !message.contains("config.json"),
            "and no file was ever written, so naming one sends the reader to the wrong place: {message}"
        );
    }
}
