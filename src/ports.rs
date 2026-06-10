//! Ports: the narrow traits that isolate effects from decisions (ADR-0003).
//! Each has exactly one real adapter (in `adapters`) and one in-memory test fake.
//!
//! The set (ARCH Canonical signatures): `LivenessProbe`, `ContainerRuntime`,
//! `NetworkProvider`, `MetadataStore`, `WorktreeProvider`, `Notifier`, `Clock`,
//! `EnvironmentProbe`, plus the ISP read-port `NotifyWatcher`.
//!
//! See backlog P-01.

use std::path::PathBuf;

use crate::domain::model::{LivenessToken, SandboxName};

/// Is this recorded anchor still the live one? Alive iff the PID exists **and**
/// its mount-namespace inode matches the token — the inode guards against PID
/// reuse. The real adapter reads `/proc`; tests script the answer.
pub trait LivenessProbe {
    fn is_alive(&self, token: &LivenessToken) -> bool;
}

/// A live anchor enumerated from the container-state registry: the sandbox
/// identity paired with the kernel liveness token its anchor runs under. The
/// cross-source reconciler reads these to spot a live anchor that no on-disk
/// record knows about (a lost record). The port that enumerates them is added
/// with the read-side ports; this is only the plain data it yields.
pub struct RegistryEntry {
    pub id: SandboxName,
    pub token: LivenessToken,
}

/// A worktree as the reconciler needs to see it, identified by its host path: a
/// record is judged inconsistent when its worktree path is absent from the
/// listed worktrees. Deliberately minimal; the listing port and the richer
/// fields (branch, dirty state) arrive with later tasks.
pub struct Worktree {
    pub path: PathBuf,
}

// TODO(P-01): the remaining port traits + plain-data types (ContainerRuntime,
//             NetworkProvider, MetadataStore, WorktreeProvider, Notifier, Clock,
//             EnvironmentProbe, NotifyWatcher, ContainerRegistry, SessionProbe,
//             OciSpec, NetworkSpec, Capabilities, DbForward, OnboardingAnswers),
//             copied verbatim from the Canonical signatures.
