//! hort: a terminal-operated, agent-agnostic manager of ephemeral sandboxes
//! (a host git worktree plus an embedded-`libcontainer` OCI container, no daemon).
//!
//! All logic lives in this library crate so the integration tests can reach it;
//! `src/main.rs` is a thin shell that parses args, assembles the real adapters,
//! and dispatches here.
//!
//! Dependencies point inward, toward the pure core:
//! - [`domain`]: pure decisions over plain data; depends on nothing external.
//! - [`ports`]: the narrow traits the domain and commands depend on (test seams).
//! - [`commands`]: coordinators wiring ports and domain into use-cases.
//! - [`adapters`]: implement the ports; the only code that touches the kernel,
//!   git, and the filesystem.
//! - [`cli`]: clap definitions and dispatch.
//!
//! The test-only `fakes` module is compiled only under test.

pub mod adapters;
pub mod cli;
pub mod commands;
pub mod domain;
pub mod ports;

#[cfg(test)]
mod fakes;
