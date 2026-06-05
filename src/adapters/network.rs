//! `PastaNetworkProvider` (`NetworkProvider`): provision/teardown host-side
//! networking — pasta always, the SNI proxy only in allowlist mode. Composes
//! `pasta` + `proxy`. Stub until the egress spike (SP-1) lands.
//!
//! See backlog A-S2 (gated).

// TODO(A-S2): hort-owned netns + container join, Landlock connect-port, pasta wiring.
