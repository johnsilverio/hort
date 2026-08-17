//! `down <name>`: destroy a sandbox pair in the mandatory teardown order.
//!
//! It looks up the record, gates on open sessions (a `--force` skips the gate; a
//! non-TTY stdin without `--force` refuses rather than guess), then executes the
//! teardown plan, dispatching each step to its port: host-side helpers stop
//! before the container, the container before its worktree, the metadata last. In
//! no-git mode the plan omits the worktree step, so the user's own folder is never
//! removed.

use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::domain::teardown::{TeardownStep, teardown_plan};
use crate::ports::{
    Confirmer, ContainerRuntime, MetadataStore, NetworkProvider, NotifyProvider, SessionProbe,
    WorktreeProvider,
};

/// Coordinates tearing a sandbox down over the ports it depends on.
pub struct DownCommand<'a> {
    store: &'a dyn MetadataStore,
    sessions: &'a dyn SessionProbe,
    confirmer: &'a dyn Confirmer,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    worktrees: &'a dyn WorktreeProvider,
    notify: &'a dyn NotifyProvider,
}

impl<'a> DownCommand<'a> {
    pub fn new(
        store: &'a dyn MetadataStore,
        sessions: &'a dyn SessionProbe,
        confirmer: &'a dyn Confirmer,
        runtime: &'a dyn ContainerRuntime,
        network: &'a dyn NetworkProvider,
        worktrees: &'a dyn WorktreeProvider,
        notify: &'a dyn NotifyProvider,
    ) -> Self {
        Self { store, sessions, confirmer, runtime, network, worktrees, notify }
    }
}

impl DownCommand<'_> {
    pub fn run(&self, name: SandboxName, force: bool, stdin_is_tty: bool) -> Result<(), HortError> {
        let record = self
            .store
            .get(&name)?
            .ok_or(HortError::UnknownSandboxOnDown { name: name.as_str().to_string() })?;

        if !force && self.has_open_sessions(&name) {
            if !stdin_is_tty {
                return Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() });
            }
            let prompt = format!("tear down sandbox '{}' with open sessions?", name.as_str());
            if !self.confirmer.confirm(&prompt)? {
                return Ok(());
            }
        }

        for step in teardown_plan(&record) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::domain::model::{AnchorPid, LivenessToken, MountNsInode, SandboxRecord};
    use crate::fakes::{
        FakeConfirmer, FakeNetwork, FakeNotifyProvider, FakeRuntime, FakeSessionProbe,
        FakeWorktreeProvider, InMemoryMetadataStore, sample_record,
    };

    fn canned_token() -> LivenessToken {
        LivenessToken { pid: AnchorPid(1234), mnt_ns: MountNsInode(5678) }
    }

    fn down_command<'a>(
        store: &'a InMemoryMetadataStore,
        sessions: &'a FakeSessionProbe,
        confirmer: &'a FakeConfirmer,
        runtime: &'a FakeRuntime,
        network: &'a FakeNetwork,
        worktrees: &'a FakeWorktreeProvider,
        notify: &'a FakeNotifyProvider,
    ) -> DownCommand<'a> {
        DownCommand { store, sessions, confirmer, runtime, network, worktrees, notify }
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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

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
        let command =
            down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees, &notify);

        command.run(name.clone(), false, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }
}
