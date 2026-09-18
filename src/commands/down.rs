//! `down <name>`: destroy a sandbox pair in the mandatory teardown order.
//!
//! Existence is two questions, because a sandbox can outlive the memory of it: a
//! record on disk, and failing that an anchor the kernel is still running under
//! that name. The second answer yields a plan that stops at the container, since
//! the worktree path was written in the record that went missing and removing one
//! would mean removing a path hort cannot name. Only a name neither source knows
//! is refused as absent.
//!
//! Whichever plan it picked, `down` gates twice, on open sessions and on a clone
//! holding commits the project repository does not have (a `--force` skips both;
//! a non-TTY stdin without `--force` refuses rather than guess), then
//! dispatches each step to its port: host-side helpers stop before the container,
//! the container before its worktree, the metadata last. In no-git mode the plan
//! omits the worktree step, so the user's own folder is never removed.

use crate::domain::config::GitMode;
use crate::domain::error::HortError;
use crate::domain::model::{SandboxName, SandboxRecord};
use crate::domain::teardown::{TeardownStep, teardown_plan, teardown_plan_without_record};
use crate::ports::{
    Confirmer, ContainerRegistry, ContainerRuntime, MetadataStore, NetworkProvider, NotifyProvider,
    SessionProbe, WorktreeProvider,
};

/// Coordinates tearing a sandbox down over the ports it depends on.
pub struct DownCommand<'a> {
    store: &'a dyn MetadataStore,
    registry: &'a dyn ContainerRegistry,
    sessions: &'a dyn SessionProbe,
    confirmer: &'a dyn Confirmer,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    worktrees: &'a dyn WorktreeProvider,
    notify: &'a dyn NotifyProvider,
}

impl<'a> DownCommand<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: &'a dyn MetadataStore,
        registry: &'a dyn ContainerRegistry,
        sessions: &'a dyn SessionProbe,
        confirmer: &'a dyn Confirmer,
        runtime: &'a dyn ContainerRuntime,
        network: &'a dyn NetworkProvider,
        worktrees: &'a dyn WorktreeProvider,
        notify: &'a dyn NotifyProvider,
    ) -> Self {
        Self { store, registry, sessions, confirmer, runtime, network, worktrees, notify }
    }
}

impl DownCommand<'_> {
    pub fn run(&self, name: SandboxName, force: bool, stdin_is_tty: bool) -> Result<(), HortError> {
        let record = self.store.get(&name)?;
        let plan = match &record {
            Some(record) => teardown_plan(record),
            None => {
                if !self.registry_knows(&name)? {
                    return Err(HortError::UnknownSandboxOnDown {
                        name: name.as_str().to_string(),
                    });
                }
                teardown_plan_without_record()
            }
        };

        if !force && self.has_open_sessions(&name) {
            if !stdin_is_tty {
                return Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() });
            }
            let prompt = format!("tear down sandbox '{}' with open sessions?", name.as_str());
            if !self.confirmer.confirm(&prompt)? {
                return Ok(());
            }
        }

        if !force && record.as_ref().is_some_and(|record| self.destroys_commits(record)) {
            if !stdin_is_tty {
                return Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() });
            }
            let prompt = format!(
                "sandbox '{}' holds commits the project repository does not have; tear it down anyway?",
                name.as_str()
            );
            if !self.confirmer.confirm(&prompt)? {
                return Ok(());
            }
        }

        for step in plan {
            match step {
                TeardownStep::StopWatcher => self.notify.teardown(&name)?,
                TeardownStep::StopNetwork => self.network.teardown(&name)?,
                TeardownStep::StopContainer => self.runtime.teardown(&name)?,
                TeardownStep::RemoveWorktree => self.worktrees.remove(&name)?,
                TeardownStep::RemoveMetadata => self.store.remove(&name)?,
            }
        }
        Ok(())
    }

    fn has_open_sessions(&self, name: &SandboxName) -> bool {
        self.sessions.session_pids(name).is_ok_and(|pids| !pids.is_empty())
    }

    /// Whether this teardown would end with commits existing nowhere. Only a
    /// `/workdir` built as a clone can hold one: a worktree commits into the
    /// project repository's own object store, so its commits outlive the box,
    /// and the mode is read from the disk at the record's own path because the
    /// record does not carry it.
    ///
    /// A read that failed asks anyway, which is the opposite of how the session
    /// gate above degrades. The costs are opposite too: read the other way round
    /// this one destroys the commits it could not see, while a session list is
    /// unreadable for every box on the machine after a reboot, and protecting
    /// there would lock the user out of their own cleanup.
    fn destroys_commits(&self, record: &SandboxRecord) -> bool {
        if !matches!(self.worktrees.git_mode_at(record.worktree_path()), Some(GitMode::Clone)) {
            return false;
        }
        self.worktrees.holds_unreturned_work(record.name()).unwrap_or(true)
    }

    /// Whether the kernel is running an anchor under this name. It is the second
    /// question `down` asks about existence, and the only one left once the
    /// metadata that remembered the sandbox is gone.
    fn registry_knows(&self, name: &SandboxName) -> Result<bool, HortError> {
        Ok(self.registry.list_live()?.iter().any(|entry| entry.id == *name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::fakes::{
        FakeConfirmer, FakeNetwork, FakeNotifyProvider, FakeRegistry, FakeRuntime,
        FakeSessionProbe, FakeWorktreeProvider, InMemoryMetadataStore, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    /// The registry of a machine running no sandbox at all, which is what every
    /// witness below but the lost-record one arranges.
    fn nothing_live() -> FakeRegistry {
        FakeRegistry::new(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    fn down_command<'a>(
        store: &'a InMemoryMetadataStore,
        registry: &'a FakeRegistry,
        sessions: &'a FakeSessionProbe,
        confirmer: &'a FakeConfirmer,
        runtime: &'a FakeRuntime,
        network: &'a FakeNetwork,
        worktrees: &'a FakeWorktreeProvider,
        notify: &'a FakeNotifyProvider,
    ) -> DownCommand<'a> {
        DownCommand { store, registry, sessions, confirmer, runtime, network, worktrees, notify }
    }

    #[test]
    fn down_tears_down_a_live_sandbox_the_store_has_no_record_of() {
        let name = SandboxName::new("ghost").unwrap();
        // The whole lost-record arrangement: the kernel is running an anchor the
        // registry can name, and the metadata that remembered it is gone.
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new().with_trace(trace.clone());
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name, false, false).unwrap();

        // The precondition `down` is written against is that the sandbox exists,
        // live or orphaned, and a box the registry names is alive by definition.
        // Refusing it leaves every host-side helper and the container standing
        // with no command on the machine able to collect them. What it stops
        // short of is the disk: the worktree path lived in the record that went
        // missing, so removing one would mean removing a path hort cannot name,
        // and there is no record left to remove.
        let expected = vec![
            "notify.teardown".to_string(),
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn down_stops_a_sandbox_whose_session_list_cannot_be_read() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::failing();
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new();
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), false, false).unwrap();

        // `down` is the one caller that keeps reading a failed session read as
        // no sessions. A reboot fails every one of these reads, so protecting
        // here would answer a piped `hort down` with a refusal for every box on
        // the machine, which locks the user out of their own cleanup.
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_tears_helpers_and_container_before_worktree() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(SandboxName::new("demo").unwrap(), false, false).unwrap();

        let expected = vec![
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
            "worktrees.remove".to_string(),
            "store.remove".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn down_stops_the_watcher_of_the_sandbox_it_tears_down() {
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new();
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(SandboxName::new("demo").unwrap(), false, false).unwrap();

        // The watcher is a host-side process outside the box, so nothing that
        // happens to the container touches it: left running, it holds the channel
        // of a sandbox that no longer exists and nothing on the machine will ever
        // stop it. The plan carries the step for every sandbox alike, and this is
        // one of the three arms that has to answer it.
        assert_eq!(notify.teardowns(), vec![SandboxName::new("demo").unwrap()]);
    }

    #[test]
    fn down_refuses_without_tty_confirmation() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![111]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), false, false);

        assert_eq!(
            result,
            Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() })
        );
        assert!(trace.borrow().is_empty());
        assert!(confirmer.prompts().is_empty());
    }

    #[test]
    fn down_leaves_the_project_folder_without_git() {
        // The record a build without git writes: no branch, and a worktree path
        // that is the user's own project folder rather than anything hort made.
        let record = SandboxRecord::new(
            SandboxName::new("demo").unwrap(),
            None,
            PathBuf::from("/home/tester/project"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-11T12:00:00Z".to_string(),
            "2026-06-11T12:00:00Z".to_string(),
            None,
            PathBuf::from("/home/tester/project"),
        );
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&record).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(SandboxName::new("demo").unwrap(), false, false).unwrap();

        let expected = vec![
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
            "store.remove".to_string(),
        ];
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn down_errors_for_unknown_name() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        let result = command.run(SandboxName::new("demo").unwrap(), false, false);

        assert_eq!(result, Err(HortError::UnknownSandboxOnDown { name: "demo".to_string() }));
        assert!(trace.borrow().is_empty());
    }

    #[test]
    fn down_prompts_before_teardown_with_open_sessions() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![111]);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new();
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), false, true).unwrap();

        assert_eq!(confirmer.prompts().len(), 1);
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_aborts_when_confirmation_declined() {
        let name = SandboxName::new("demo").unwrap();
        let record = sample_record("demo");
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&record).unwrap();
        let sessions = FakeSessionProbe::new(vec![111]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        let result = command.run(name.clone(), false, true);

        assert_eq!(result, Ok(()));
        assert!(trace.borrow().is_empty());
        assert_eq!(store.get(&name).unwrap(), Some(record));
    }

    #[test]
    fn down_skips_confirmation_with_force() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![111]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new();
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), true, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_asks_before_tearing_down_a_clone_holding_work_the_host_repository_lacks() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees =
            FakeWorktreeProvider::new().with_clone_workdir(&name).with_unreturned_work(&name);
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name, false, true).unwrap();

        // A clone keeps the commits made inside it, so this teardown is the one
        // that ends with the work existing nowhere. Naming the box is intent to
        // destroy the sandbox, never intent to destroy work the host never saw.
        assert_eq!(confirmer.prompts().len(), 1);
    }

    #[test]
    fn down_leaves_the_clone_standing_when_the_prompt_is_declined() {
        let name = SandboxName::new("demo").unwrap();
        let record = sample_record("demo");
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&record).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_unreturned_work(&name)
            .with_trace(trace.clone());
        let notify = FakeNotifyProvider::new().with_trace(trace.clone());
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        let result = command.run(name.clone(), false, true);

        assert_eq!(result, Ok(()));
        assert!(trace.borrow().is_empty());
        assert_eq!(store.get(&name).unwrap(), Some(record));
    }

    #[test]
    fn down_refuses_a_clone_holding_work_the_host_repository_lacks_without_a_terminal() {
        let name = SandboxName::new("demo").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_unreturned_work(&name)
            .with_trace(trace.clone());
        let notify = FakeNotifyProvider::new().with_trace(trace.clone());
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        let result = command.run(name, false, false);

        assert_eq!(
            result,
            Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() })
        );
        assert!(trace.borrow().is_empty());
    }

    #[test]
    fn down_asks_when_it_could_not_tell_whether_the_clone_holds_unreturned_work() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::yes();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_failing_unreturned_work_probe(&name);
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name, false, true).unwrap();

        // Read the other way round, a failed read destroys the commits it could
        // not see, and asking costs a keystroke on a box the user is downing
        // anyway. This is where `down` parts from its own session gate, which
        // degrades the opposite way because every one of those reads fails after
        // a reboot and refusing them all would lock the user out of cleanup.
        assert_eq!(confirmer.prompts().len(), 1);
    }

    #[test]
    fn down_does_not_ask_when_the_host_repository_already_has_the_clones_work() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new().with_clone_workdir(&name);
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), false, true).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_does_not_ask_about_unreturned_work_in_worktree_mode() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        // A worktree commits into the project repository's own object store, so
        // its commits outlive this teardown whatever a probe of the box says.
        let worktrees =
            FakeWorktreeProvider::new().with_listed_worktree(&name).with_unreturned_work(&name);
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), false, true).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_does_not_ask_about_unreturned_work_when_no_record_names_the_clone() {
        let name = SandboxName::new("ghost").unwrap();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        let registry = FakeRegistry::new(vec![(name.clone(), canned_token())]);
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new()
            .with_clone_workdir(&name)
            .with_unreturned_work(&name)
            .with_trace(trace.clone());
        let notify = FakeNotifyProvider::new().with_trace(trace.clone());
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name, false, false).unwrap();

        // The path of the `/workdir` lived in the record that went missing, so
        // there is no clone hort can name to ask about, and a guard that
        // guessed one would refuse the only route this box has to collection.
        // Nothing on disk is at stake either: this plan stops at the container,
        // leaving the directory and its commits exactly where they are.
        let expected = vec![
            "notify.teardown".to_string(),
            "network.teardown".to_string(),
            "runtime.teardown".to_string(),
        ];
        assert!(confirmer.prompts().is_empty());
        assert_eq!(*trace.borrow(), expected);
    }

    #[test]
    fn down_skips_the_unreturned_work_prompt_with_force() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees =
            FakeWorktreeProvider::new().with_clone_workdir(&name).with_unreturned_work(&name);
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), true, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }

    #[test]
    fn down_proceeds_without_prompt_when_no_sessions() {
        let name = SandboxName::new("demo").unwrap();
        let store = InMemoryMetadataStore::new();
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token());
        let network = FakeNetwork::new();
        let worktrees = FakeWorktreeProvider::new();
        let notify = FakeNotifyProvider::new();
        let registry = nothing_live();
        let command = down_command(
            &store, &registry, &sessions, &confirmer, &runtime, &network, &worktrees, &notify,
        );

        command.run(name.clone(), false, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }
}
