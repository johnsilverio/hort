//! Adapters: the only code that touches the world (kernel, `/proc`, git, the
//! filesystem, external binaries). Each implements a port from `ports`
//! (architecture.md). Kernel adapters land as `todo!()` stubs gated on spikes.

pub mod environment;
pub mod liveness;
pub mod metadata;
pub mod network;
pub mod notify;
pub mod pasta;
pub mod prompt;
pub mod proxy;
pub mod runtime;
pub mod worktree;
