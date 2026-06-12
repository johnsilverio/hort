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
    Confirmer, ContainerRuntime, MetadataStore, NetworkProvider, SessionProbe, WorktreeProvider,
};

/// Coordinates tearing a sandbox down over the ports it depends on.
pub struct DownCommand<'a> {
    store: &'a dyn MetadataStore,
    sessions: &'a dyn SessionProbe,
    confirmer: &'a dyn Confirmer,
    runtime: &'a dyn ContainerRuntime,
    network: &'a dyn NetworkProvider,
    worktrees: &'a dyn WorktreeProvider,
}

impl<'a> DownCommand<'a> {
    pub fn new(
        store: &'a dyn MetadataStore,
        sessions: &'a dyn SessionProbe,
        confirmer: &'a dyn Confirmer,
        runtime: &'a dyn ContainerRuntime,
        network: &'a dyn NetworkProvider,
        worktrees: &'a dyn WorktreeProvider,
    ) -> Self {
        Self { store, sessions, confirmer, runtime, network, worktrees }
    }
}

impl DownCommand<'_> {
    pub fn run(
        &self,
        name: SandboxName,
        force: bool,
        stdin_is_tty: bool,
    ) -> Result<(), HortError> {
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
                // No watcher pid is persisted yet, so there is nothing to stop;
                // the watcher-stop seam lands with the notify watcher work.
                TeardownStep::StopWatcher => {}
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
        FakeConfirmer, FakeNetwork, FakeRuntime, FakeSessionProbe, FakeWorktreeProvider,
        InMemoryMetadataStore, sample_record,
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
    ) -> DownCommand<'a> {
        DownCommand { store, sessions, confirmer, runtime, network, worktrees }
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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
    fn down_refuses_without_tty_confirmation() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&sample_record("demo")).unwrap();
        let sessions = FakeSessionProbe::new(vec![111]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

        let result = command.run(SandboxName::new("demo").unwrap(), false, false);

        assert_eq!(result, Err(HortError::RefusedWithoutConfirmation { command: "down".to_string() }));
        assert!(trace.borrow().is_empty());
        assert!(confirmer.prompts().is_empty());
    }

    #[test]
    fn down_no_git_keeps_user_folder() {
        let record = SandboxRecord::new(
            SandboxName::new("demo").unwrap(),
            None,
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-11T12:00:00Z".to_string(),
            "2026-06-11T12:00:00Z".to_string(),
            None,
        );
        let trace = Rc::new(RefCell::new(Vec::new()));
        let store = InMemoryMetadataStore::new().with_trace(trace.clone());
        store.put(&record).unwrap();
        let sessions = FakeSessionProbe::new(vec![]);
        let confirmer = FakeConfirmer::no();
        let runtime = FakeRuntime::new(canned_token()).with_trace(trace.clone());
        let network = FakeNetwork::new().with_trace(trace.clone());
        let worktrees = FakeWorktreeProvider::new().with_trace(trace.clone());
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

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
        let command = down_command(&store, &sessions, &confirmer, &runtime, &network, &worktrees);

        command.run(name.clone(), false, false).unwrap();

        assert!(confirmer.prompts().is_empty());
        assert_eq!(store.get(&name).unwrap(), None);
    }
}
