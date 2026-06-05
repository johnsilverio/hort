//! Ports: the narrow traits that isolate effects from decisions (ADR-0003).
//! Each has exactly one real adapter (in `adapters`) and one in-memory test fake.
//!
//! The set (ARCH Canonical signatures): `LivenessProbe`, `ContainerRuntime`,
//! `NetworkProvider`, `MetadataStore`, `WorktreeProvider`, `Notifier`, `Clock`,
//! `EnvironmentProbe`, plus the ISP read-port `NotifyWatcher`.
//!
//! See backlog P-01.

// TODO(P-01): the port traits + plain-data types (OciSpec, NetworkSpec,
//             Capabilities, Worktree, DbForward, Warning, OnboardingAnswers),
//             copied verbatim from the ARCH Canonical signatures block.
