//! `doctor`: read-only per-capability report from the `EnvironmentProbe`; exit
//! non-zero when a hard precondition (user namespaces, pasta) is missing, so it
//! works as a scriptable health gate. Post-skeleton (ADR-0012).
//!
//! See backlog C-07.

// TODO(C-07): the DoctorCommand coordinator.
