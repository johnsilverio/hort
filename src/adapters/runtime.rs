//! `LibcontainerRuntime` (`ContainerRuntime`): start the anchor, join sessions
//! via `TenantContainerBuilder`, tear down the container only — NOT the
//! host-side helpers. Stub until the runtime/overlay spike (SP-2) lands.
//!
//! See backlog A-S1 (gated).

// TODO(A-S1): real libcontainer runtime; `resources.cpus` → `cpu.max` (SPEC-9).
