//! Ports: the narrow traits that isolate effects from decisions (ADR-0003).
//! Each has exactly one real adapter (in `adapters`) and one in-memory test fake.
//!
//! The set (ARCH Canonical signatures): `LivenessProbe`, `ContainerRuntime`,
//! `NetworkProvider`, `MetadataStore`, `WorktreeProvider`, `Notifier`, `Clock`,
//! `EnvironmentProbe`, plus the ISP read-port `NotifyWatcher`.
//!
//! See backlog P-01.

use crate::domain::model::LivenessToken;

/// Is this recorded anchor still the live one? Alive iff the PID exists **and**
/// its mount-namespace inode matches the token — the inode guards against PID
/// reuse. The real adapter reads `/proc`; tests script the answer.
pub trait LivenessProbe {
    fn is_alive(&self, token: &LivenessToken) -> bool;
}

// TODO(P-01): the remaining port traits + plain-data types — ContainerRuntime,
//             NetworkProvider, MetadataStore, WorktreeProvider, Notifier, Clock,
//             EnvironmentProbe, NotifyWatcher, ContainerRegistry, SessionProbe,
//             OciSpec, NetworkSpec, Capabilities, Worktree, DbForward,
//             OnboardingAnswers — copied verbatim from the Canonical signatures.
