//! Domain newtypes and records: `SandboxName`, `BranchName`, `Domain`,
//! `AnchorPid`, `MountNsInode`, `LivenessToken`, `SandboxRecord`, `Capabilities`.
//! Validated newtypes make illegal states unrepresentable (ADR-0008).
//!
//! See backlog D-01, D-06.

// TODO(D-01/D-06): validated newtypes (`::new -> Result`), the `LivenessToken`
//                  value object, and the persisted `SandboxRecord` (private fields).
