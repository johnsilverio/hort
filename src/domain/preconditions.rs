//! Precondition selection: can this host and this configured rootfs build a
//! sandbox, and if not, which error does the user get? A pure decision over the
//! plain facts the environment probe collects, so the commands only run effects.

use std::path::PathBuf;

use crate::domain::egress::EgressPolicy;
use crate::domain::error::HortError;
use crate::domain::model::Capabilities;

/// What the host says about the one rootfs the config resolved. The facts carry
/// their own path, so "configured but never inspected" cannot be represented.
pub struct RootfsFacts {
    pub path: PathBuf,
    pub exists: bool,
    pub has_default_shell: bool,
    pub configured_shell: Option<ConfiguredShell>,
    pub workdir_writable: bool,
}

/// The session shell the configuration declares, and whether the rootfs provides
/// it. Absent from the facts when the configuration declares no `shell`, which is
/// the common case and raises nothing.
pub struct ConfiguredShell {
    pub path: String,
    pub present: bool,
}

/// The shell a prepared rootfs is required to carry, which is what makes it the
/// answer hort can always fall back to.
const DEFAULT_SHELL: &str = "/bin/sh";

/// Select the precondition error `up` must raise before building anything, or
/// `None` to proceed. Checks run in order: user namespaces, then pasta, then
/// git, then `ip` when the egress posture is an allowlist, then the rootfs
/// chain. The first four say this host cannot run this sandbox, the rest say
/// this configuration is wrong. `ip` is asked for only under an allowlist,
/// because that is the only posture whose route tables have to be emptied.
///
/// git is asked for in both project modes, not only the one that builds a
/// worktree, because hort asks git whether the project is a repository before
/// it can tell the two apart. A host without the binary therefore also defeats
/// the arm that exists for a project holding no repository at all.
pub fn up_precondition_error(
    caps: &Capabilities,
    egress: &EgressPolicy,
    rootfs: Option<&RootfsFacts>,
) -> Option<HortError> {
    if !caps.user_ns {
        return Some(HortError::UserNamespacesDisabled);
    }

    if caps.pasta.is_none() {
        return Some(HortError::PastaMissing);
    }

    if !caps.git {
        return Some(HortError::GitMissing);
    }

    if matches!(egress, EgressPolicy::Allowlist(_)) && caps.ip.is_none() {
        return Some(HortError::IpMissing);
    }

    rootfs_precondition_error(rootfs)
}

/// Select the error one prepared rootfs raises, or `None` when it can carry a
/// sandbox. Checks run in order: resolved at all, on disk, carrying the default
/// shell, carrying the shell the configuration declares, and `/workdir`
/// writable by the mapped uid. `None` facts mean nothing resolved a rootfs,
/// which is itself an error.
///
/// It stands apart from the host chain above because the two answer different
/// questions and not every caller asks both. Building a sandbox needs the host
/// to be capable first; onboarding asks only whether the directory somebody
/// just typed can build one, and reporting a kernel setting there would answer
/// a question nobody asked, before the path was even looked at.
pub fn rootfs_precondition_error(rootfs: Option<&RootfsFacts>) -> Option<HortError> {
    let Some(rootfs) = rootfs else {
        return Some(HortError::NoRootfsConfigured);
    };

    let path = rootfs.path.display().to_string();

    if !rootfs.exists {
        return Some(HortError::RootfsMissing { path });
    }

    if !rootfs.has_default_shell {
        return Some(HortError::RootfsWithoutShell { path });
    }

    if let Some(shell) = &rootfs.configured_shell
        && !shell.present
    {
        return Some(HortError::ShellNotInRootfs { shell: shell.path.clone(), path });
    }

    if !rootfs.workdir_writable {
        return Some(HortError::WorkdirNotWritable { path });
    }

    None
}

/// Select the precondition error `attach` must raise before joining a sandbox,
/// or `None` to proceed.
///
/// It asks about user namespaces and about nothing else. What entering a running
/// sandbox needs is the namespace join: the host-side helpers are already up and
/// the container already exists with its root filesystem mounted, so refusing
/// entry to a live sandbox because pasta left the `PATH` would be hort inventing
/// an obstacle rather than reporting one.
pub fn attach_precondition_error(caps: &Capabilities) -> Option<HortError> {
    (!caps.user_ns).then_some(HortError::UserNamespacesDisabled)
}

/// Whether this host meets the preconditions that no configuration can supply:
/// unprivileged user namespaces, pasta and git. `hort doctor` leaves with
/// success on a host that meets them and with a failure on one that does not,
/// so a script can gate on it.
///
/// It is a predicate and never an error, because the report is what `doctor`
/// exists to produce and an error would take the report away on the very arm a
/// caller wrote the gate to read. It asks about the host and nothing else: a
/// rootfs that is not configured yet is the ordinary state of a machine that has
/// just been set up, and closing the gate on it would tell the user their kernel
/// is the problem.
pub fn hard_preconditions_are_met(caps: &Capabilities) -> bool {
    caps.user_ns && caps.pasta.is_some() && caps.git
}

/// The shell a session execs, from the configuration, the shell the user runs on
/// the host, and whether that host shell resolves inside the sandbox's rootfs.
///
/// The configured shell wins as declared, since a build already refused one the
/// rootfs does not carry. The host shell is taken only when the rootfs provides
/// it, because a session that execs a shell the box does not have opens nothing.
/// Everything else lands on the default, which the rootfs contract obliges a
/// prepared rootfs to carry: a project with no rootfs configured, a rootfs that
/// has left the disk and a read that could not answer all arrive here, because
/// entering a sandbox never validates a rootfs and so must not die for failing to
/// look inside one.
pub fn session_shell(
    configured: Option<&str>,
    host_shell: Option<&str>,
    host_shell_in_rootfs: bool,
) -> String {
    if let Some(shell) = configured {
        return shell.to_string();
    }
    match host_shell {
        Some(shell) if host_shell_in_rootfs => shell.to_string(),
        _ => DEFAULT_SHELL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::egress::HostPattern;
    use crate::domain::model::{CgroupCaps, Domain};

    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            git: true,
        }
    }

    fn valid_rootfs() -> RootfsFacts {
        RootfsFacts {
            path: PathBuf::from("/opt/hort/rootfs"),
            exists: true,
            has_default_shell: true,
            configured_shell: Some(ConfiguredShell {
                path: "/bin/bash".to_string(),
                present: true,
            }),
            workdir_writable: true,
        }
    }

    fn allowlist() -> EgressPolicy {
        EgressPolicy::Allowlist(vec![HostPattern::Exact(Domain::new("api.anthropic.com").unwrap())])
    }

    #[test]
    fn preconditions_select_user_namespaces_disabled() {
        let caps = Capabilities { user_ns: false, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn preconditions_select_pasta_missing() {
        let caps = Capabilities { pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::PastaMissing));
    }

    #[test]
    fn preconditions_select_no_rootfs_configured() {
        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, None);

        assert_eq!(error, Some(HortError::NoRootfsConfigured));
    }

    #[test]
    fn preconditions_select_missing_rootfs_directory() {
        let rootfs = RootfsFacts {
            path: PathBuf::from("/opt/hort/rootfs"),
            exists: false,
            has_default_shell: false,
            configured_shell: Some(ConfiguredShell {
                path: "/bin/zsh".to_string(),
                present: false,
            }),
            workdir_writable: false,
        };

        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&rootfs));

        assert_eq!(error, Some(HortError::RootfsMissing { path: "/opt/hort/rootfs".to_string() }));
    }

    #[test]
    fn preconditions_select_rootfs_without_shell() {
        let rootfs = RootfsFacts { has_default_shell: false, ..valid_rootfs() };

        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&rootfs));

        assert_eq!(
            error,
            Some(HortError::RootfsWithoutShell { path: "/opt/hort/rootfs".to_string() })
        );
    }

    #[test]
    fn preconditions_select_configured_shell_missing() {
        let rootfs = RootfsFacts {
            configured_shell: Some(ConfiguredShell {
                path: "/bin/zsh".to_string(),
                present: false,
            }),
            ..valid_rootfs()
        };

        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&rootfs));

        assert_eq!(
            error,
            Some(HortError::ShellNotInRootfs {
                shell: "/bin/zsh".to_string(),
                path: "/opt/hort/rootfs".to_string(),
            })
        );
    }

    #[test]
    fn preconditions_select_workdir_not_writable() {
        let rootfs = RootfsFacts { workdir_writable: false, ..valid_rootfs() };

        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&rootfs));

        assert_eq!(
            error,
            Some(HortError::WorkdirNotWritable { path: "/opt/hort/rootfs".to_string() })
        );
    }

    #[test]
    fn preconditions_select_no_error_for_a_ready_host() {
        let error =
            up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, None);
    }

    #[test]
    fn preconditions_prefer_user_namespaces_over_pasta() {
        let caps = Capabilities { user_ns: false, pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn preconditions_prefer_pasta_over_rootfs() {
        let caps = Capabilities { pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, None);

        assert_eq!(error, Some(HortError::PastaMissing));
    }

    #[test]
    fn up_errors_when_git_is_absent_from_the_host() {
        let caps = Capabilities { git: false, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::GitMissing));
    }

    #[test]
    fn preconditions_prefer_user_namespaces_over_git() {
        let caps = Capabilities { user_ns: false, git: false, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        // Installing a binary is a smaller answer than a kernel that builds no
        // sandbox at all, and a host missing both has to hear the larger one
        // first or it repairs the smaller and gets refused again.
        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn up_reports_the_missing_git_before_a_missing_rootfs() {
        let caps = Capabilities { git: false, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, None);

        assert_eq!(error, Some(HortError::GitMissing));
    }

    #[test]
    fn up_errors_when_ip_is_missing_and_egress_is_an_allowlist() {
        let caps = Capabilities { ip: None, ..ready_host() };

        let error = up_precondition_error(&caps, &allowlist(), Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::IpMissing));
    }

    #[test]
    fn up_proceeds_without_ip_when_egress_is_open() {
        let caps = Capabilities { ip: None, ..ready_host() };

        let error = up_precondition_error(&caps, &EgressPolicy::Open, Some(&valid_rootfs()));

        assert_eq!(error, None);
    }

    #[test]
    fn up_reports_the_missing_ip_before_a_missing_rootfs() {
        let caps = Capabilities { ip: None, ..ready_host() };

        let error = up_precondition_error(&caps, &allowlist(), None);

        assert_eq!(error, Some(HortError::IpMissing));
    }

    #[test]
    fn attach_preconditions_select_user_namespaces_disabled() {
        let caps = Capabilities { user_ns: false, ..ready_host() };

        let error = attach_precondition_error(&caps);

        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn attach_preconditions_ignore_the_host_tooling_a_build_needs() {
        let caps = Capabilities { pasta: None, ip: None, ..ready_host() };

        let error = attach_precondition_error(&caps);

        // Both binaries belong to whoever provisions the networking, and by the
        // time anyone joins a sandbox its helpers are already running. Refusing
        // to enter a live box over either of them locks the user out of work
        // that is sitting there uncommitted.
        assert_eq!(error, None);
    }

    #[test]
    fn doctor_gate_opens_on_a_host_that_can_build_a_sandbox() {
        assert!(hard_preconditions_are_met(&ready_host()));
    }

    #[test]
    fn doctor_gate_closes_when_pasta_is_absent() {
        let caps = Capabilities { pasta: None, ..ready_host() };

        assert!(!hard_preconditions_are_met(&caps));
    }

    #[test]
    fn doctor_gate_closes_when_git_is_absent() {
        let caps = Capabilities { git: false, ..ready_host() };

        assert!(!hard_preconditions_are_met(&caps));
    }

    #[test]
    fn doctor_gate_closes_when_user_namespaces_are_disabled() {
        let caps = Capabilities { user_ns: false, ..ready_host() };

        assert!(!hard_preconditions_are_met(&caps));
    }

    #[test]
    fn session_shell_prefers_the_configured_shell_over_the_host_shell() {
        let shell = session_shell(Some("/usr/bin/fish"), Some("/bin/bash"), true);

        assert_eq!(shell, "/usr/bin/fish");
    }

    #[test]
    fn session_shell_takes_the_host_shell_when_the_rootfs_provides_it() {
        let shell = session_shell(None, Some("/bin/bash"), true);

        assert_eq!(shell, "/bin/bash");
    }

    #[test]
    fn session_shell_falls_back_when_the_rootfs_lacks_the_host_shell() {
        let shell = session_shell(None, Some("/bin/bash"), false);

        // The shell a person runs on their own machine is very often one the
        // prepared rootfs never had installed, and a session told to exec it
        // opens nothing at all.
        assert_eq!(shell, "/bin/sh");
    }

    #[test]
    fn preconditions_ignore_the_shell_check_when_config_declares_none() {
        let rootfs = RootfsFacts { configured_shell: None, ..valid_rootfs() };

        let error = up_precondition_error(&ready_host(), &EgressPolicy::Open, Some(&rootfs));

        assert_eq!(error, None);
    }
}
