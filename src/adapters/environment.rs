//! `EnvironmentProbe` real adapter: read user namespaces, pasta on PATH, the
//! delegated cgroup controllers, the Landlock ABI, rootless overlayfs,
//! notify-send, git, and rootfs validity into `Capabilities`. Read-only; never
//! fails (absent ⇒ recorded absent).
//!
//! See backlog A-05.

// TODO(A-05): the host/kernel capability detection.
