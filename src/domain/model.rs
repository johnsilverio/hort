//! Domain newtypes: `SandboxName`, `BranchName`, `Domain`, `AnchorPid`,
//! `MountNsInode`, `LivenessToken`. A validated newtype makes an illegal value
//! unrepresentable: its constructor returns `Err` instead of wrapping bad input.

use crate::domain::error::HortError;

/// A sandbox identity (the tmux-style name from `hort up <name>`). Validated at
/// construction: non-empty and usable as both a git branch name and a single
/// directory component, so `/` is rejected. You cannot hold an invalid one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxName(String);

impl SandboxName {
    /// Validate `value` as a sandbox name and wrap it.
    pub fn new(value: &str) -> Result<Self, HortError> {
        if value.is_empty() || value.contains('/') {
            return Err(HortError::InvalidName);
        }
        Ok(Self(value.to_owned()))
    }
}

/// A git branch name. Validated at construction: non-empty. Unlike a sandbox
/// name a `/` is allowed, since git branches are hierarchical.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchName(String);

impl BranchName {
    /// Validate `value` as a git branch name and wrap it.
    pub fn new(value: &str) -> Result<Self, HortError> {
        if value.is_empty() {
            return Err(HortError::InvalidName);
        }
        Ok(Self(value.to_owned()))
    }
}

/// An egress-allowlist hostname: a bare hostname, with no scheme or path. The
/// `*.` wildcard is handled by the egress `HostPattern`, not here, so a `Domain`
/// never carries a `*`. Validated at construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Domain(String);

impl Domain {
    /// Validate `value` as a bare hostname and wrap it.
    pub fn new(value: &str) -> Result<Self, HortError> {
        if is_hostname(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(HortError::InvalidName)
        }
    }
}

fn is_hostname(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(is_hostname_label)
}

fn is_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The anchor process id. Thin wrapper, constructed by tuple (no validation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnchorPid(u32);

/// The mount-namespace inode at `/proc/<pid>/ns/mnt`; guards against PID reuse.
/// Thin wrapper, constructed by tuple (no validation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MountNsInode(u64);

/// The kernel liveness token of a sandbox: the anchor PID plus the
/// mount-namespace inode (the inode guards PID reuse). A sandbox is alive iff the
/// PID exists *and* the inode matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LivenessToken {
    pub pid: AnchorPid,
    pub mnt_ns: MountNsInode,
}

// TODO(D-06): the persisted `SandboxRecord` (private fields) + `Capabilities`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_name_rejects_empty() {
        assert!(matches!(SandboxName::new(""), Err(HortError::InvalidName)));
    }

    #[test]
    fn sandbox_name_rejects_slash() {
        assert!(matches!(
            SandboxName::new("feature/x"),
            Err(HortError::InvalidName)
        ));
    }

    #[test]
    fn sandbox_name_accepts_valid() {
        assert!(SandboxName::new("feature-x").is_ok());
    }

    #[test]
    fn branch_name_accepts_valid() {
        assert!(BranchName::new("feature-x").is_ok());
    }

    #[test]
    fn domain_rejects_non_hostname() {
        assert!(matches!(
            Domain::new("https://api.anthropic.com/v1"),
            Err(HortError::InvalidName)
        ));
    }

    #[test]
    fn domain_accepts_valid_hostname() {
        assert!(Domain::new("api.anthropic.com").is_ok());
    }

    #[test]
    fn domain_rejects_empty_label() {
        assert!(matches!(Domain::new("api..com"), Err(HortError::InvalidName)));
    }

    #[test]
    fn domain_rejects_leading_hyphen_label() {
        assert!(matches!(Domain::new("-api.com"), Err(HortError::InvalidName)));
    }

    #[test]
    fn domain_rejects_trailing_hyphen_label() {
        assert!(matches!(Domain::new("api-.com"), Err(HortError::InvalidName)));
    }
}
