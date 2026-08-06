//! Adapters: the only code that touches the world (kernel, `/proc`, git, the
//! filesystem, external binaries). Each implements a port from `ports`.

pub mod clock;
pub mod config;
pub mod confirm;
pub mod console;
pub mod environment;
pub mod forwarder;
pub mod helper;
pub mod landlock;
pub mod liveness;
pub mod lock;
pub mod metadata;
pub mod namespaces;
pub mod network;
pub mod notify;
pub mod pasta;
pub mod prompt;
pub mod proxy;
pub mod runtime;
pub mod streams;
pub mod terminal;
pub mod worktree;
