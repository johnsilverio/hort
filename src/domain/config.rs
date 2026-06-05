//! Config as pure parse + pure merge (ADR-0007): JSONC `&str -> Config` via
//! `json-strip-comments` + `serde_json` into `#[serde(rename_all = "camelCase")]`
//! structs, then `merge(global, local)` with local winning (PRD §4.1).
//! Locating/reading the files is a thin reader in `adapters`, not here.
//!
//! See backlog D-03, D-04.

// TODO(D-03/D-04): the camelCase config structs, the JSONC parse, and the merge
//                  semantics (scalars + egress replace; additive arrays
//                  union+dedupe; objects deep-merge).
