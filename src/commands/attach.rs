//! `attach <name>`: open one more session inside a sandbox that is already
//! running.
//!
//! It never creates anything. A name nothing knows and a name whose anchor is
//! gone are two different answers, and both stop here. A live sandbox gets a
//! session, which is a process joined to its namespaces running the configured
//! login shell in the worktree, and the attach is timestamped so a forgotten
//! sandbox can still be told from a busy one.

use std::path::PathBuf;

use crate::domain::config::ResolvedConfig;
use crate::domain::error::HortError;
use crate::domain::model::{SandboxName, SandboxRecord};
use crate::domain::reconcile::SandboxState;
use crate::ports::{Clock, ContainerRuntime, LivenessProbe, MetadataStore, SessionSpec};

/// Where every session starts, which is the sandbox's own mount point for the
/// worktree rather than anything the configuration can move.
const WORKDIR: &str = "/workdir";
/// The shell a prepared rootfs is required to carry, which is what makes it the
/// answer hort can always fall back to.
const DEFAULT_SHELL: &str = "/bin/sh";

/// Coordinates opening a session in a running sandbox over the ports it depends
/// on. What the session runs comes from the resolved configuration, where it
/// runs comes from the record.
pub struct AttachCommand<'a> {
    store: &'a dyn MetadataStore,
    probe: &'a dyn LivenessProbe,
    runtime: &'a dyn ContainerRuntime,
    clock: &'a dyn Clock,
    config: &'a ResolvedConfig,
}

impl AttachCommand<'_> {
    /// Open a session in the sandbox named `name`, returning the host pid the
    /// session process runs under so the caller can wait for it.
    pub fn run(&self, name: SandboxName) -> Result<u32, HortError> {
        let record = self
            .store
            .get(&name)?
            .ok_or(HortError::UnknownSandboxOnAttach { name: name.as_str().to_string() })?;

        if record.reconcile(self.probe) != SandboxState::Live {
            return Err(HortError::SandboxNotRunning { name: name.as_str().to_string() });
        }

        let session = self.runtime.join_session(&SessionSpec {
            name,
            command: self.login_shell(),
            cwd: PathBuf::from(WORKDIR),
            env: sandbox_environment(&record),
        })?;

        // Timestamped only once a session exists, because this is what tells an
        // idle sandbox from a busy one: an attach that failed to open anything
        // would otherwise make a forgotten sandbox look freshly used.
        let attached_at = humantime::format_rfc3339(self.clock.now()).to_string();
        self.store.put(&record.with_last_attach_at(attached_at))?;
        Ok(session)
    }

    fn login_shell(&self) -> Vec<String> {
        let shell = self.config.shell.as_deref().unwrap_or(DEFAULT_SHELL);
        vec![shell.to_string(), "-l".to_string()]
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

    use crate::domain::config::{Cache, Mounts};
    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::domain::reconcile::SandboxState;
    use crate::fakes::{
        FakeRuntime, InMemoryMetadataStore, ScriptedClock, ScriptedLivenessProbe, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn live_record() -> SandboxRecord {
        sample_record("demo").with_token(canned_token())
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

    fn attach_command<'a>(
        store: &'a InMemoryMetadataStore,
        probe: &'a ScriptedLivenessProbe,
        runtime: &'a FakeRuntime,
        clock: &'a ScriptedClock,
        config: &'a ResolvedConfig,
    ) -> AttachCommand<'a> {
        AttachCommand { store, probe, runtime, clock, config }
    }

    #[test]
    fn attach_opens_a_session_in_a_live_sandbox() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(SandboxName::new("demo").unwrap()).unwrap();

        assert_eq!(runtime.joins(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn attach_errors_not_running_for_an_orphaned_record() {
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let probe = ScriptedLivenessProbe::new(false);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        let result = command.run(SandboxName::new("demo").unwrap());

        assert_eq!(result, Err(HortError::SandboxNotRunning { name: "demo".to_string() }));
        assert!(runtime.joins().is_empty());
    }

    #[test]
    fn attach_errors_absent_for_an_unknown_name() {
        let store = InMemoryMetadataStore::new();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        let result = command.run(SandboxName::new("demo").unwrap());

        assert_eq!(result, Err(HortError::UnknownSandboxOnAttach { name: "demo".to_string() }));
        assert!(runtime.joins().is_empty());
    }

    #[test]
    fn attach_runs_the_configured_shell_as_a_login_shell() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(Some("/usr/bin/fish"));
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(SandboxName::new("demo").unwrap()).unwrap();

        assert_eq!(runtime.session_command(), vec!["/usr/bin/fish".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_falls_back_to_the_default_shell_when_none_is_configured() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(SandboxName::new("demo").unwrap()).unwrap();

        // The one shell a prepared rootfs is required to carry, which is what
        // makes it the answer hort can always fall back to.
        assert_eq!(runtime.session_command(), vec!["/bin/sh".to_string(), "-l".to_string()]);
    }

    #[test]
    fn attach_starts_the_session_in_the_worktree() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(SandboxName::new("demo").unwrap()).unwrap();

        assert_eq!(runtime.session_cwd(), PathBuf::from("/workdir"));
    }

    #[test]
    fn attach_exports_the_sandbox_environment_to_the_session() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(SandboxName::new("demo").unwrap()).unwrap();

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
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(name.clone()).unwrap();

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
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        command.run(name.clone()).unwrap();

        // Timestamping the attach makes it the second writer of a record only a
        // build used to touch, and one that rebuilds the record around the new
        // timestamp drops the anchor it never knew about: from then on a running
        // sandbox reads as orphaned, which is what prune removes without asking.
        let persisted = store.get(&name).unwrap().unwrap();
        assert_eq!(persisted.reconcile(&probe), SandboxState::Live);
    }

    #[test]
    fn attach_reports_the_process_the_session_runs_as() {
        let store = InMemoryMetadataStore::new();
        store.put(&live_record()).unwrap();
        let probe = ScriptedLivenessProbe::new(true);
        let runtime = FakeRuntime::new(canned_token());
        let clock = ScriptedClock::new(humantime::parse_rfc3339("2026-06-11T13:00:00Z").unwrap());
        let config = config_with_shell(None);
        let command = attach_command(&store, &probe, &runtime, &clock, &config);

        let session = command.run(SandboxName::new("demo").unwrap()).unwrap();

        // The session is the process the user is in, and nothing else hands its
        // pid back: a caller with no pid to wait for returns to the host prompt
        // while the shell it just opened is still running.
        assert_eq!(session, FakeRuntime::SESSION_PID);
    }
}
