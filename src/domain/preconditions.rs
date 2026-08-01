//! Precondition selection: can this host and this configured rootfs build a
//! sandbox, and if not, which error does the user get? A pure decision over the
//! plain facts the environment probe collects, so the commands only run effects.

use std::path::PathBuf;

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

/// Select the precondition error `up` must raise before building anything, or
/// `None` to proceed. Checks run in order: user namespaces, then pasta, then the
/// rootfs chain (configured, exists, default shell, configured shell, `/workdir`
/// writable). The first two say this host cannot run hort at all, the rest say
/// this configuration is wrong. A `rootfs` of `None` means the merged config
/// resolved no rootfs, which is itself an error.
pub fn up_precondition_error(
    caps: &Capabilities,
    rootfs: Option<&RootfsFacts>,
) -> Option<HortError> {
    if !caps.user_ns {
        return Some(HortError::UserNamespacesDisabled);
    }

    if caps.pasta.is_none() {
        return Some(HortError::PastaMissing);
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::CgroupCaps;

    fn ready_host() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: false },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: true,
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

    #[test]
    fn preconditions_select_user_namespaces_disabled() {
        let caps = Capabilities { user_ns: false, ..ready_host() };

        let error = up_precondition_error(&caps, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn preconditions_select_pasta_missing() {
        let caps = Capabilities { pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::PastaMissing));
    }

    #[test]
    fn preconditions_select_no_rootfs_configured() {
        let error = up_precondition_error(&ready_host(), None);

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

        let error = up_precondition_error(&ready_host(), Some(&rootfs));

        assert_eq!(error, Some(HortError::RootfsMissing { path: "/opt/hort/rootfs".to_string() }));
    }

    #[test]
    fn preconditions_select_rootfs_without_shell() {
        let rootfs = RootfsFacts { has_default_shell: false, ..valid_rootfs() };

        let error = up_precondition_error(&ready_host(), Some(&rootfs));

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

        let error = up_precondition_error(&ready_host(), Some(&rootfs));

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

        let error = up_precondition_error(&ready_host(), Some(&rootfs));

        assert_eq!(
            error,
            Some(HortError::WorkdirNotWritable { path: "/opt/hort/rootfs".to_string() })
        );
    }

    #[test]
    fn preconditions_select_no_error_for_a_ready_host() {
        let error = up_precondition_error(&ready_host(), Some(&valid_rootfs()));

        assert_eq!(error, None);
    }

    #[test]
    fn preconditions_prefer_user_namespaces_over_pasta() {
        let caps = Capabilities { user_ns: false, pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, Some(&valid_rootfs()));

        assert_eq!(error, Some(HortError::UserNamespacesDisabled));
    }

    #[test]
    fn preconditions_prefer_pasta_over_rootfs() {
        let caps = Capabilities { pasta: None, ..ready_host() };

        let error = up_precondition_error(&caps, None);

        assert_eq!(error, Some(HortError::PastaMissing));
    }

    #[test]
    fn preconditions_ignore_the_shell_check_when_config_declares_none() {
        let rootfs = RootfsFacts { configured_shell: None, ..valid_rootfs() };

        let error = up_precondition_error(&ready_host(), Some(&rootfs));

        assert_eq!(error, None);
    }
}
