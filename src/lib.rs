//! hort — a terminal-operated, agent-agnostic manager of ephemeral sandboxes
//! (a host git worktree + an embedded-`libcontainer` OCI container, no daemon).
//!
//! All logic lives in this library crate; `src/main.rs` is a thin shell that
//! parses args, assembles the real adapters, and dispatches here (ADR-0002).
//!
//! Layers, with dependencies pointing INWARD (architecture.md):
//! - [`domain`]   — pure decisions over plain data; depends on nothing external.
//! - [`ports`]    — the narrow traits the domain/commands depend on (test seams).
//! - [`commands`] — coordinators wiring ports + domain into use-cases.
//! - [`adapters`] — the only code that touches the kernel, git, the filesystem.
//! - [`cli`]      — clap definitions + dispatch.

pub mod adapters;
pub mod cli;
pub mod commands;
pub mod domain;
pub mod ports;
