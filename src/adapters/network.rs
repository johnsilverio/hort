//! `NullNetwork`: the honest stand-in for the host-side egress wiring until the
//! pasta-backed provider lands. Its teardown is a no-op because there is nothing
//! to tear down yet, and provisioning reports that the runtime is not available,
//! since there is no container to wire egress for in this build. Networking stays
//! a port of its own even in the null, separate from the runtime.

use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::ports::{NetworkProvider, NetworkSpec};

/// A `NetworkProvider` for builds without the pasta-backed egress wiring.
pub struct NullNetwork;

impl NetworkProvider for NullNetwork {
    fn provision(&self, _spec: &NetworkSpec) -> Result<(), HortError> {
        Err(HortError::RuntimeUnavailable)
    }

    fn teardown(&self, _name: &SandboxName) -> Result<(), HortError> {
        Ok(())
    }
}
