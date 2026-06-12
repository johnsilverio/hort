//! `NullRuntime`: the honest stand-in for the container runtime until the
//! embedded one lands. It serves every runtime-side port the commands wire, so
//! the binary can dispatch `ls` and `down` for real while any operation that
//! would start a container reports that the runtime is not available.
//!
//! The reads are empty because no hort container can exist before a real runtime
//! starts one, and the teardown is a no-op because there is nothing to tear down
//! yet. The embedded runtime replaces this in place, taking over the registry
//! and session reads as well.

use crate::domain::error::HortError;
use crate::domain::model::{LivenessToken, SandboxName};
use crate::ports::{ContainerRegistry, ContainerRuntime, OciSpec, RegistryEntry, SessionProbe};

/// A `ContainerRuntime` (and the read ports the embedded runtime will also serve)
/// for builds without the in-process container runtime.
pub struct NullRuntime;

impl ContainerRuntime for NullRuntime {
    fn start_anchor(&self, _spec: &OciSpec) -> Result<LivenessToken, HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn join_session(&self, _name: &SandboxName) -> Result<(), HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn teardown(&self, _name: &SandboxName) -> Result<(), HortError> {
        Ok(())
    }
}

impl ContainerRegistry for NullRuntime {
    fn list_live(&self) -> Result<Vec<RegistryEntry>, HortError> {
        Ok(Vec::new())
    }
}

impl SessionProbe for NullRuntime {
    fn session_pids(&self, _name: &SandboxName) -> Result<Vec<u32>, HortError> {
        Ok(Vec::new())
    }
}
