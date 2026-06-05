//! `ProcLivenessProbe` (`LivenessProbe`): stat `/proc/<pid>/ns/mnt` and compare
//! its inode against the token (guards PID reuse). Testable without root against
//! the test's own PID. No extra crate — `std::os::linux::fs::MetadataExt::ino()`.
//!
//! See backlog A-03.

// TODO(A-03): the /proc liveness probe + shared contract test.
