//! Pure domain: decisions over plain data, no I/O. Depends on nothing external
//! and must never `use` an adapter, `libcontainer`, `git`, or `std::fs` state
//! (architecture.md red flags). Carries the bulk of the tests (testing.md §4.1).

pub mod config;
pub mod egress;
pub mod error;
pub mod idle;
pub mod model;
pub mod onboarding;
pub mod policy;
pub mod reconcile;
pub mod teardown;
