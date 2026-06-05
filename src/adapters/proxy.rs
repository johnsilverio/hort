//! The SNI/CONNECT egress proxy: ClientHello SNI peek, NO MITM — accept CONNECT
//! by hostname, match the allowlist, splice bytes through untouched. Spawned only
//! in allowlist mode. Stub until SP-1 (SPEC-2: rustls vs tls-parser).
//!
//! See backlog A-S2 (gated).

// TODO(A-S2): the no-MITM SNI proxy.
