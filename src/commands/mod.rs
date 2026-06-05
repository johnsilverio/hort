//! Commands: the coordinators that wire ports + domain into use-cases
//! (`up`/`attach`/`ls`/`down`/`prune`/`config`/`doctor`). They depend on the
//! domain plus trait ports, never on a concrete adapter (architecture.md).

pub mod attach;
pub mod config;
pub mod doctor;
pub mod down;
pub mod ls;
pub mod prune;
pub mod up;
