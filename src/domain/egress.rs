//! `EgressPolicy { Open, Allowlist(Vec<Domain>) }` and `matches(host) -> bool`.
//! Decides whether a proxy is needed and which hostnames pass — it never spawns
//! anything (ADR-0004). Match rule pending SPEC-4 (default: exact match).
//!
//! See backlog D-05.

// TODO(D-05): the policy enum + `matches`.
