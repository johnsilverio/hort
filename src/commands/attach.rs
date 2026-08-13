//! `attach <name>`: open one more session inside a sandbox that is already
//! running.
//!
//! It never creates anything. A name nothing knows and a name whose anchor is
//! gone are two different answers, and both stop here. A live sandbox gets a
//! session, which is a process joined to its namespaces running a login shell in
//! the worktree: the configured shell, else the one the user runs on the host
//! when the sandbox's root filesystem carries it, else the shell every prepared
//! rootfs has. The attach is timestamped so a forgotten sandbox can still be
//! told from a busy one.

use std::path::{Path, PathBuf};

use crate::domain::config::ResolvedConfig;
use crate::domain::error::HortError;
use crate::domain::model::{SandboxName, SandboxRecord, Warning};
use crate::domain::preconditions::{attach_precondition_error, session_shell};
use crate::domain::reconcile::SandboxState;
use crate::ports::{
    Clock, ContainerRuntime, EnvironmentProbe, LivenessProbe, MetadataStore, ProxyEndpoint,
    Session, SessionSpec,
};

/// Where every session starts, which is the sandbox's own mount point for the
/// worktree rather than anything the configuration can move.
const WORKDIR: &str = "/workdir";
/// The address the sandbox reaches its own host-side helpers on, which is where
/// its proxy answers and where every declared database is published.
const LOOPBACK: &str = "127.0.0.1";
/// The addresses a session reaches without going through the proxy. Both
/// spellings are named because this list is matched against the address a tool
/// was handed and never against what that address resolves to, so the dotted
/// form on its own exempts nothing that a person addressed by name.
const PROXY_EXEMPTIONS: &str = "127.0.0.1,localhost";

/// Coordinates opening a session in a running sandbox over the ports it depends
/// on. What the session runs comes from the resolved configuration, where it
/// runs comes from the record.
///
/// It is also the one place a session's environment is assembled, and that is
/// mechanism rather than layout: a session that asks for a terminal is described
/// to the runtime by a process file that replaces what the runtime would have
/// assembled, so it inherits nothing the sandbox itself declared. Anything drawn
/// from the configuration reaches the shell a person types into only from here.
pub struct AttachCommand<'a> {
    store: &'a dyn MetadataStore,
    probe: &'a dyn LivenessProbe,
    runtime: &'a dyn ContainerRuntime,
    clock: &'a dyn Clock,
    env: &'a dyn EnvironmentProbe,
    proxy: &'a dyn ProxyEndpoint,
    config: &'a ResolvedConfig,
    /// The shell the user runs on the host, read where every other host variable
    /// is read, which is the assembly of the real adapters.
    host_shell: Option<String>,
    /// The environment hort itself was invoked with, which is where a declared
    /// credential variable is looked up.
    host_env: Vec<(String, String)>,
}

impl<'a> AttachCommand<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: &'a dyn MetadataStore,
        probe: &'a dyn LivenessProbe,
        runtime: &'a dyn ContainerRuntime,
        clock: &'a dyn Clock,
        env: &'a dyn EnvironmentProbe,
        proxy: &'a dyn ProxyEndpoint,
        config: &'a ResolvedConfig,
        host_shell: Option<String>,
        host_env: Vec<(String, String)>,
    ) -> Self {
        Self { store, probe, runtime, clock, env, proxy, config, host_shell, host_env }
    }
}

impl AttachCommand<'_> {
    /// Open a session in the sandbox named `name`, returning it so the caller can
    /// hold its terminal and wait for it, along with whatever the session had to
    /// go without.
    ///
    /// `terminal` says whether the session gets a pty of the sandbox's own.
    /// Whether there is a terminal to relay is a fact about the process that
    /// invoked hort, so the caller decides it and this only carries it.
    pub fn run(
        &self,
        name: SandboxName,
        terminal: bool,
    ) -> Result<(Session, Vec<Warning>), HortError> {
        if let Some(error) = attach_precondition_error(&self.env.detect()) {
            return Err(error);
        }

        let record = self
            .store
            .get(&name)?
            .ok_or(HortError::UnknownSandboxOnAttach { name: name.as_str().to_string() })?;

        if record.reconcile(self.probe) != SandboxState::Live {
            return Err(HortError::SandboxNotRunning { name: name.as_str().to_string() });
        }

        let (credentials, warnings) = self.declared_credentials();
        // The runtime reads these pairs in order and the later assignment is the
        // one the process ends up with, so what hort states about the sandbox
        // comes after what the configuration asked to be forwarded into it.
        let env =
            [credentials, self.proxy_environment(&name), sandbox_environment(&record)].concat();

        let session = self.runtime.join_session(&SessionSpec {
            name,
            command: self.login_shell(),
            cwd: PathBuf::from(WORKDIR),
            env,
            terminal,
        })?;

        // Timestamped only once a session exists, because this is what tells an
        // idle sandbox from a busy one: an attach that failed to open anything
        // would otherwise make a forgotten sandbox look freshly used.
        let attached_at = humantime::format_rfc3339(self.clock.now()).to_string();
        self.store.put(&record.with_last_attach_at(attached_at))?;
        Ok((session, warnings))
    }

    fn login_shell(&self) -> Vec<String> {
        let shell = session_shell(
            self.config.shell.as_deref(),
            self.host_shell.as_deref(),
            self.host_shell_is_in_the_rootfs(),
        );
        vec![shell, "-l".to_string()]
    }

    /// Whether the shell the user runs on the host resolves inside the sandbox's
    /// root filesystem. A configuration that names no rootfs, and a rootfs the
    /// host cannot answer for, both read as absent, because entering a sandbox
    /// validates no root filesystem and so must not die for failing to look
    /// inside one.
    fn host_shell_is_in_the_rootfs(&self) -> bool {
        let (Some(rootfs), Some(host_shell)) =
            (self.config.rootfs.as_deref(), self.host_shell.as_deref())
        else {
            return false;
        };
        self.env
            .inspect_rootfs(Path::new(rootfs), Some(host_shell))
            .configured_shell
            .is_some_and(|shell| shell.present)
    }

    /// The credentials the configured agents declared, read from the environment
    /// hort was invoked with, and a warning naming each one this host does not
    /// have.
    fn declared_credentials(&self) -> (Vec<(String, String)>, Vec<Warning>) {
        let mut forwarded = Vec::new();
        let mut warnings = Vec::new();
        for declared in self.config.agents.iter().flat_map(|agent| &agent.auth.env) {
            match self.host_env.iter().find(|(name, _)| name == declared) {
                Some(pair) => forwarded.push(pair.clone()),
                // Skipped rather than forwarded empty, which is worse than
                // absent: the tool reads a credential it has and reports back
                // that the credential was rejected.
                None => warnings.push(Warning::new(format!(
                    "environment variable '{declared}' is not set on this host, so the session starts without it"
                ))),
            }
        }
        (forwarded, warnings)
    }

    /// Where the session sends the traffic it cannot send itself, which is
    /// nowhere for a sandbox that has no proxy running.
    ///
    /// The running proxy decides this and never the configured posture. The
    /// configuration is read fresh on every attach while the sandbox was built
    /// once, so a file edited to an allowlist under a standing open box would
    /// otherwise hand every tool in it an address nobody listens on.
    fn proxy_environment(&self, name: &SandboxName) -> Vec<(String, String)> {
        let Some(port) = self.proxy.proxy_port(name) else {
            return Vec::new();
        };
        let proxy = format!("http://{LOOPBACK}:{port}");
        vec![
            ("HTTP_PROXY".to_string(), proxy.clone()),
            ("HTTPS_PROXY".to_string(), proxy.clone()),
            ("ALL_PROXY".to_string(), proxy),
            // Every declared database is published on the sandbox's own loopback,
            // so exempting loopback exempts all of them at once. Sent to the
            // proxy instead, a database is refused for not being a TLS host.
            ("NO_PROXY".to_string(), PROXY_EXEMPTIONS.to_string()),
        ]
    }
}

/// What every session in a sandbox sees of it, which is the data a shell prompt
/// renders the sandbox from. hort exports it and draws nothing itself.
fn sandbox_environment(record: &SandboxRecord) -> Vec<(String, String)> {
    vec![
        ("HORT_SANDBOX".to_string(), record.name().as_str().to_string()),
        ("HORT_WORKTREE".to_string(), record.worktree_path().display().to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::domain::config::{Agent, Auth, Cache, Egress, Mounts};
    use crate::domain::model::{
        AnchorPid, Capabilities, CgroupCaps, LivenessToken, MountNsInode, SandboxRecord,
    };
    use crate::domain::reconcile::SandboxState;
    use crate::fakes::{
        FakeCapabilities, FakeProxyEndpoint, FakeRuntime, InMemoryMetadataStore, ScriptedClock,
        ScriptedLivenessProbe, sample_record,
    };

    /// The loopback port a sandbox's proxy answers on in these tests.
    const PROXY_PORT: u16 = 4242;

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn live_record() -> SandboxRecord {
        sample_record("demo").with_token(canned_token())
    }

    /// A host that can run hort: user namespaces, and the tooling a build needs.
    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: true,
            git: true,
        }
    }

    fn config_with_shell(shell: Option<&str>) -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some("/base/rootfs".to_string()),
            agents: Vec::new(),
            mounts: Mounts::default(),
            network: Vec::new(),
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: shell.map(str::to_owned),
            resources: None,
        }
    }

    /// One agent that keeps its credentials in the named host variables.
    fn agent_forwarding(variables: &[&str]) -> Agent {
        Agent {
            command: "claude".to_string(),
            auth: Auth {
                read_only: Vec::new(),
                env: variables.iter().map(|name| (*name).to_string()).collect(),
            },
            notify: None,
        }
    }

    /// A project that admits one host, which is the posture the proxy exists in.
    fn allowlist() -> Option<Egress> {
        Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] })
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_command<'a>(
        store: &'a InMemoryMetadataStore,
        probe: &'a ScriptedLivenessProbe,
        runtime: &'a FakeRuntime,
        clock: &'a ScriptedClock,
        env: &'a FakeCapabilities,
        proxy: &'a FakeProxyEndpoint,
        config: &'a ResolvedConfig,
        host_shell: Option<&str>,
        host_env: Vec<(String, String)>,
    ) -> AttachCommand<'a> {
        AttachCommand {
            store,
            probe,
            runtime,
            clock,
            env,
            proxy,
            config,
            host_shell: host_shell.map(str::to_owned),
            host_env,
        }
    }

    #[test]
    fn attach_opens_a_session_in_a_live_sandbox() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        assert_eq!(runtime.joins(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn attach_errors_not_running_for_an_orphaned_record() {
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let probe = ScriptedLivenessProbe::new(false);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        let result = command.run(SandboxName::new("demo").unwrap(), false);

        assert_eq!(result.unwrap_err(), HortError::SandboxNotRunning { name: "demo".to_string() });
        assert!(runtime.joins().is_empty());
    }

    #[test]
    fn attach_errors_absent_for_an_unknown_name() {
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        let result = command.run(SandboxName::new("demo").unwrap(), false);

        assert_eq!(
            result.unwrap_err(),
            HortError::UnknownSandboxOnAttach { name: "demo".to_string() }
        );
        assert!(runtime.joins().is_empty());
    }

    #[test]
    fn attach_runs_the_configured_shell_as_a_login_shell() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(Some("/usr/bin/fish"));
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        assert_eq!(runtime.session_command(), vec!["/usr/bin/fish".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_falls_back_to_the_default_shell_when_the_rootfs_lacks_the_host_shell() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host()).without_the_shell_asked_about();
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            Some("/usr/bin/fish"),
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // The shell a person runs on their own machine is very often one the
        // prepared rootfs never had installed, so taking it on the user's word
        // opens a session on a program the box cannot exec. The default is the
        // one shell the rootfs contract obliges, which is what makes the fall
        // safe.
        assert_eq!(runtime.session_command(), vec!["/bin/sh".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_runs_the_host_shell_when_the_rootfs_provides_it() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            Some("/usr/bin/fish"),
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // A user who never configured a shell still gets the one they type in
        // every day, as long as the box has it, which is the whole point of
        // asking the rootfs instead of guessing either way.
        assert_eq!(runtime.session_command(), vec!["/usr/bin/fish".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_opens_a_session_without_a_rootfs_configured() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig { rootfs: None, ..config_with_shell(None) };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            Some("/usr/bin/fish"),
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // Entering a sandbox validates no root filesystem, because the box was
        // built out of one long ago and is standing with it mounted. A run that
        // died here would lock the user out of a live sandbox over a key that
        // only matters when building.
        assert_eq!(runtime.session_command(), vec!["/bin/sh".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_refuses_a_host_without_user_namespaces() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(Capabilities { user_ns: false, ..ready_host() });
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        let result = command.run(SandboxName::new("demo").unwrap(), false);

        // A session is a process joined to the sandbox's namespaces, so a kernel
        // that hands out no user namespaces cannot open one. Left to the runtime
        // this surfaces as whatever the join happens to fail with, which names
        // neither the cause nor the repair.
        assert_eq!(result.unwrap_err(), HortError::UserNamespacesDisabled);
        assert!(runtime.joins().is_empty());
    }

    #[test]
    fn attach_points_the_session_at_the_sandbox_proxy_under_an_allowlist() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::listening_on(PROXY_PORT);
        let config = ResolvedConfig { egress: allowlist(), ..config_with_shell(None) };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // Under an allowlist the proxy is the only way out of the namespace, so
        // a tool that was never told about it opens a socket that reaches
        // nothing, and the box reads as broken rather than as restricted.
        let environment = runtime.session_env();
        let proxy_url = format!("http://127.0.0.1:{PROXY_PORT}");
        assert!(environment.contains(&("HTTP_PROXY".to_string(), proxy_url.clone())));
        assert!(environment.contains(&("HTTPS_PROXY".to_string(), proxy_url.clone())));
        assert!(environment.contains(&("ALL_PROXY".to_string(), proxy_url)));
    }

    #[test]
    fn attach_exempts_loopback_from_the_proxy() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::listening_on(PROXY_PORT);
        let config = ResolvedConfig { egress: allowlist(), ..config_with_shell(None) };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // Every declared database answers on the sandbox's own loopback, so
        // naming loopback names all of them. Without the exemption a database
        // client sends its traffic to the proxy, which refuses it for not being
        // an allowlisted TLS host.
        let environment = runtime.session_env();
        let (_, exemptions) = environment
            .iter()
            .find(|(name, _)| name == "NO_PROXY")
            .expect("the addresses a session reaches without the proxy");
        assert!(exemptions.contains("127.0.0.1"));
    }

    #[test]
    fn attach_exempts_loopback_by_name_and_not_only_by_address() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::listening_on(PROXY_PORT);
        let config = ResolvedConfig { egress: allowlist(), ..config_with_shell(None) };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // The exemption list is matched against the address a tool was given,
        // never against what that address resolves to, so the dotted form
        // exempts only the tools that were handed the dotted form. A client
        // pointed at a local service by name is otherwise sent to the proxy,
        // which refuses it, and the box looks broken while doing exactly what
        // it was told.
        let environment = runtime.session_env();
        let (_, exemptions) = environment
            .iter()
            .find(|(name, _)| name == "NO_PROXY")
            .expect("the addresses a session reaches without the proxy");
        assert!(exemptions.contains("localhost"));
    }

    #[test]
    fn attach_keeps_the_sandbox_proxy_over_a_declared_one_of_the_same_name() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::listening_on(PROXY_PORT);
        let config = ResolvedConfig {
            agents: vec![agent_forwarding(&["HTTP_PROXY"])],
            egress: allowlist(),
            ..config_with_shell(None)
        };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            vec![("HTTP_PROXY".to_string(), "http://proxy.corp:3128".to_string())],
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // Forwarding the host's own proxy into the box is a plausible thing to
        // declare and a fatal thing to honour: that address belongs to a network
        // the sandbox has no route to, so the one way out of a closed namespace
        // would point somewhere unreachable and every request would fail with
        // nothing in the proxy log to explain it.
        let environment = runtime.session_env();
        let last = environment.iter().rfind(|(name, _)| name == "HTTP_PROXY");
        assert_eq!(
            last,
            Some(&("HTTP_PROXY".to_string(), format!("http://127.0.0.1:{PROXY_PORT}")))
        );
    }

    #[test]
    fn attach_names_no_proxy_in_the_open_posture() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // An open sandbox has no proxy at all, and pointing its tools at one
        // that was never started breaks a box that otherwise works.
        let environment = runtime.session_env();
        assert!(!environment.iter().any(|(name, _)| name.ends_with("_PROXY")));
    }

    #[test]
    fn attach_names_no_proxy_for_a_sandbox_that_has_none_running() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig { egress: allowlist(), ..config_with_shell(None) };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // The configuration is read fresh on every attach while the sandbox was
        // built once, so the two disagree the moment someone edits the file
        // under a running box. What decides here is the sandbox: a proxy that
        // was never started has no port to send anything to, and an address
        // invented to satisfy the file points every tool in the box at nothing.
        let environment = runtime.session_env();
        assert!(!environment.iter().any(|(name, _)| name.ends_with("_PROXY")));
    }

    #[test]
    fn attach_forwards_a_declared_auth_variable_from_the_host() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig {
            agents: vec![agent_forwarding(&["ANTHROPIC_API_KEY"])],
            ..config_with_shell(None)
        };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            vec![("ANTHROPIC_API_KEY".to_string(), "sk-live".to_string())],
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // A session that asks for a terminal is described by a process file that
        // replaces what the runtime would have assembled, so nothing the sandbox
        // declared reaches it. A credential declared in the configuration exists
        // for the shell a person types into only because it is put here.
        assert!(
            runtime
                .session_env()
                .contains(&("ANTHROPIC_API_KEY".to_string(), "sk-live".to_string()))
        );
    }

    #[test]
    fn attach_skips_a_declared_auth_variable_the_host_does_not_have() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig {
            agents: vec![agent_forwarding(&["ANTHROPIC_API_KEY"])],
            ..config_with_shell(None)
        };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // Declared and missing is the ordinary state of a machine where the user
        // logs the agent in instead of exporting a key, so it can never be
        // fatal. Handed over empty it is worse than absent: the tool reads a
        // credential it has and reports it as rejected.
        assert!(!runtime.session_env().iter().any(|(name, _)| name == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn attach_warns_about_a_declared_auth_variable_the_host_does_not_have() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig {
            agents: vec![agent_forwarding(&["ANTHROPIC_API_KEY"])],
            ..config_with_shell(None)
        };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        let (_, warnings) = command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // A credential that silently failed to arrive costs the user an hour
        // hunting for why the agent asks them to log in, in a box where nothing
        // else looks wrong.
        assert!(warnings.iter().any(|warning| warning.to_string().contains("ANTHROPIC_API_KEY")));
    }

    #[test]
    fn attach_applies_its_own_pairs_after_a_declared_auth_variable() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = ResolvedConfig {
            agents: vec![agent_forwarding(&["HORT_SANDBOX"])],
            ..config_with_shell(None)
        };
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            vec![("HORT_SANDBOX".to_string(), "somewhere-else".to_string())],
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // The runtime reads these pairs in order and the later assignment is the
        // one a process ends up with, so what hort states about the sandbox has
        // to come last. Declared first, a forwarded variable would rename the
        // box in the prompt of the shell standing in it.
        assert_eq!(
            runtime.session_env(),
            vec![
                ("HORT_SANDBOX".to_string(), "somewhere-else".to_string()),
                ("HORT_SANDBOX".to_string(), "demo".to_string()),
                ("HORT_WORKTREE".to_string(), "/state/sandboxes/demo/worktree-demo".to_string()),
            ]
        );
    }

    #[test]
    fn attach_starts_the_session_in_the_worktree() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        assert_eq!(runtime.session_cwd(), PathBuf::from("/workdir"));
    }

    #[test]
    fn attach_exports_the_sandbox_environment_to_the_session() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        let environment = runtime.session_env();
        assert!(environment.contains(&("HORT_SANDBOX".to_string(), "demo".to_string())));
        assert!(environment.contains(&(
            "HORT_WORKTREE".to_string(),
            "/state/sandboxes/demo/worktree-demo".to_string()
        )));
    }

    #[test]
    fn attach_records_the_time_of_the_attach() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(name.clone(), false).unwrap();

        // This timestamp is what tells an idle sandbox from a busy one once the
        // sessions of an attach are gone, so a session that opens without
        // leaving one behind ages the sandbox from the wrong moment.
        let persisted = store.get(&name).unwrap().unwrap();
        assert_eq!(persisted.last_attach_at(), "2026-06-11T13:00:00Z");
    }

    #[test]
    fn attach_leaves_the_record_reconciling_as_live() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(name.clone(), false).unwrap();

        // Timestamping the attach makes it the second writer of a record only a
        // build used to touch, and one that rebuilds the record around the new
        // timestamp drops the anchor it never knew about: from then on a running
        // sandbox reads as orphaned, which is what prune removes without asking.
        let persisted = store.get(&name).unwrap().unwrap();
        assert_eq!(persisted.reconcile(&probe), SandboxState::Live);
    }

    #[test]
    fn attach_asks_the_sandbox_for_a_terminal_when_the_caller_has_one() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), true).unwrap();

        // The pty belongs to the sandbox, and the session that runs without one
        // gets the terminal hort was invoked on instead: a process inside the box
        // can then write to the terminal the user is sitting at, and on a host
        // that still allows it, push keystrokes the user's shell runs once hort
        // is gone.
        assert!(runtime.session_terminal());
    }

    #[test]
    fn attach_asks_for_no_terminal_when_the_caller_has_none() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // With no terminal on this side there is no pty to relay and nothing to
        // protect, so the session runs on the stdio it inherits. Asking for one
        // anyway would leave a sandbox holding a master nobody reads, which is a
        // shell that blocks on a full buffer for a run nobody is watching.
        assert!(!runtime.session_terminal());
    }

    #[test]
    fn attach_reports_the_process_the_session_runs_as() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let env = FakeCapabilities::new(ready_host());
        let proxy = FakeProxyEndpoint::without_proxy();
        let config = config_with_shell(None);
        let command = attach_command(
            &store,
            &probe,
            &runtime,
            &clock,
            &env,
            &proxy,
            &config,
            None,
            Vec::new(),
        );

        let (session, _) = command.run(SandboxName::new("demo").unwrap(), false).unwrap();

        // The session is the process the user is in, and nothing else hands its
        // pid back: a caller with no pid to wait for returns to the host prompt
        // while the shell it just opened is still running.
        assert_eq!(session.pid, FakeRuntime::SESSION_PID);
    }
}
