//! Domain newtypes: `SandboxName`, `BranchName`, `Domain`, `AnchorPid`,
//! `MountNsInode`, `LivenessToken`, plus the `Warning` advisory message. A
//! validated newtype makes an illegal value unrepresentable: its constructor
//! returns `Err` instead of wrapping bad input.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::error::HortError;

/// A sandbox identity (the tmux-style name from `hort up <name>`). Validated at
/// construction: non-empty and usable as both a git branch name and a single
/// directory component, so `/` is rejected. You cannot hold an invalid one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxName(String);

impl SandboxName {
    /// Validate `value` as a sandbox name and wrap it.
    pub fn new(value: &str) -> Result<Self, HortError> {
        if value.is_empty() || value.contains('/') {
            return Err(HortError::InvalidName);
        }
        Ok(Self(value.to_owned()))
    }

    /// The wrapped sandbox name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A git branch name. Validated at construction: non-empty. Unlike a sandbox
/// name a `/` is allowed, since git branches are hierarchical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BranchName(String);

impl BranchName {
    /// Validate `value` as a git branch name and wrap it.
    pub fn new(value: &str) -> Result<Self, HortError> {
        if value.is_empty() {
            return Err(HortError::InvalidName);
        }
        Ok(Self(value.to_owned()))
    }

    /// The wrapped branch name.
    pub fn as_str(&self) -> &str {
        &self.0
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

    /// The wrapped hostname.
    pub fn as_str(&self) -> &str {
        &self.0
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnchorPid(pub u32);

/// The mount-namespace inode at `/proc/<pid>/ns/mnt`; guards against PID reuse.
/// Thin wrapper, constructed by tuple (no validation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MountNsInode(pub u64);

/// The kernel liveness token of a sandbox: the anchor PID plus the
/// mount-namespace inode (the inode guards PID reuse). A sandbox is alive iff the
/// PID exists *and* the inode matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LivenessToken {
    pub pid: AnchorPid,
    #[serde(rename = "mntNsInode")]
    pub mnt_ns: MountNsInode,
}

/// A non-fatal advisory surfaced to the user: a config key hort cannot honor, a
/// `devcontainer.json` field it ignores while mapping, or a host capability it
/// could not detect during onboarding. A plain message wrapper, not a validated
/// newtype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning(String);

impl Warning {
    /// Wrap a human-readable warning message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The persisted memory of a sandbox: the intent recorded when `up` builds it,
/// plus the kernel liveness token filled in once the anchor is running. It is a
/// cache of intent, never the authority on liveness. The kernel process table
/// holds that truth. Serialized to `metadata.json` with camelCase keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRecord {
    schema_version: u32,
    name: SandboxName,
    branch: Option<BranchName>,
    worktree_path: PathBuf,
    overlay_path: PathBuf,
    created_at: String,
    last_attach_at: String,
    notify_channel: Option<String>,
    watcher_pid: Option<u32>,
    token: Option<LivenessToken>,
}

impl SandboxRecord {
    /// Build a fresh pre-anchor record from the intent known at `up` time. A
    /// `branch` of `None` is no-git mode, where hort created no disposable
    /// worktree. The liveness token starts `None` because the anchor has not been
    /// started yet; call [`with_token`](SandboxRecord::with_token) once it is
    /// running.
    pub fn new(
        name: SandboxName,
        branch: Option<BranchName>,
        worktree_path: PathBuf,
        overlay_path: PathBuf,
        created_at: String,
        last_attach_at: String,
        notify_channel: Option<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            name,
            branch,
            worktree_path,
            overlay_path,
            created_at,
            last_attach_at,
            notify_channel,
            watcher_pid: None,
            token: None,
        }
    }

    /// Record the running anchor's liveness token (its PID and mount-namespace
    /// inode), returning the updated record to persist.
    pub fn with_token(self, token: LivenessToken) -> Self {
        Self { token: Some(token), ..self }
    }

    /// Record the PID of the host-side notify watcher `up` spawned, returning the
    /// updated record to persist. A record carries one only when a notify channel
    /// is configured, so its presence is what later marks a watcher to stop.
    pub fn with_watcher_pid(self, pid: u32) -> Self {
        Self { watcher_pid: Some(pid), ..self }
    }

    /// The sandbox identity this record belongs to.
    pub fn name(&self) -> &SandboxName {
        &self.name
    }

    /// The branch hort checked out in the worktree, or `None` in no-git mode
    /// (where there is no disposable worktree to remove on teardown).
    pub fn branch(&self) -> Option<&BranchName> {
        self.branch.as_ref()
    }

    /// The host path of this sandbox's worktree.
    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// The RFC 3339 creation timestamp, as persisted.
    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    /// The RFC 3339 timestamp of the most recent attach, as persisted.
    pub fn last_attach_at(&self) -> &str {
        &self.last_attach_at
    }

    /// The PID of the host-side notify watcher, or `None` when no notify channel
    /// is configured and none was spawned.
    pub fn watcher_pid(&self) -> Option<u32> {
        self.watcher_pid
    }

    /// The kernel liveness token, or `None` before the anchor has started.
    pub fn liveness_token(&self) -> Option<LivenessToken> {
        self.token
    }
}

/// What the host and kernel can actually do, as detected by the environment
/// probe. The single source for `up`/`attach` preconditions, per-controller
/// cgroup degradation, and the onboarding generator. Rootfs validity is not here:
/// validating one needs the configured rootfs path, which is config rather than a
/// host fact.
#[derive(Clone)]
pub struct Capabilities {
    pub user_ns: bool,
    pub pasta: Option<PathBuf>,
    pub cgroup: CgroupCaps,
    pub landlock_abi: Option<u8>,
    pub overlayfs_rootless: bool,
    pub notify_send: bool,
    pub git: bool,
}

/// Which cgroup v2 controllers are delegated to hort's user slice. The resource
/// ceiling caps what is present and degrades with a warning for what is missing.
#[derive(Clone)]
pub struct CgroupCaps {
    pub memory: bool,
    pub pids: bool,
    pub cpu: bool,
    pub cpuset: bool,
}

/// The answers the onboarding flow collects to drive config generation. An empty
/// placeholder for now; its fields arrive with the onboarding command.
pub struct OnboardingAnswers;

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

    #[test]
    fn record_token_is_none_before_anchor() {
        let record = SandboxRecord::new(
            SandboxName::new("demo").unwrap(),
            Some(BranchName::new("demo").unwrap()),
            PathBuf::from("/state/sandboxes/demo/worktree-demo"),
            PathBuf::from("/state/sandboxes/demo/overlay"),
            "2026-06-10T12:00:00Z".to_string(),
            "2026-06-10T12:00:00Z".to_string(),
            None,
        );

        assert_eq!(record.liveness_token(), None);
    }
}
