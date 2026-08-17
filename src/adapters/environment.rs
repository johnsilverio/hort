//! `HostEnvironmentProbe`: the environment probe that reads the real host. It
//! answers two questions and never fails, so a capability it cannot observe is
//! recorded absent and a path it cannot read yields the conservative fact.
//!
//! User namespaces are observed by trying, never by reading a sysctl: no sysctl
//! for them is carried by every distribution, and some restrict unprivileged user
//! namespaces through a security profile that no sysctl reflects.
//!
//! Rootless overlay is an indication rather than a proof: `overlay` listed in
//! `/proc/filesystems` plus working user namespaces says the kernel offers the
//! filesystem, never that a particular mount would succeed.
//!
//! Paths inside a rootfs are resolved from the rootfs root and never through the
//! host, because a prepared rootfs commonly ships `/bin/sh` as an absolute
//! symlink whose target exists only inside that rootfs.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::ptr;

use crate::domain::model::{Capabilities, CgroupCaps};
use crate::domain::mounts::MountSourceFacts;
use crate::domain::preconditions::{ConfiguredShell, RootfsFacts};
use crate::ports::EnvironmentProbe;

const PROC_FILESYSTEMS: &str = "/proc/filesystems";
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";
const PROC_MOUNTS: &str = "/proc/mounts";
const CGROUP2_FSTYPE: &str = "cgroup2";
const DEFAULT_CGROUP2_MOUNT: &str = "/sys/fs/cgroup";
const CONTROLLERS_FILE: &str = "cgroup.controllers";
const DEFAULT_SHELL: &str = "/bin/sh";
const WORKDIR: &str = "workdir";
const EXECUTE_BITS: u32 = 0o111;
const WORLD_WRITE: u32 = 0o002;
/// How many symlinks one resolution may expand before it gives up, in the spirit
/// of the kernel's own `ELOOP` ceiling: a rootfs whose links form a cycle answers
/// absent instead of hanging the probe.
const MAX_LINK_HOPS: usize = 8;
/// Asking `landlock_create_ruleset` for the ABI version rather than for a
/// ruleset. `libc` carries the syscall number but not this flag.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_ulong = 1;

/// An `EnvironmentProbe` that reads the live host: `/proc`, `PATH`, the delegated
/// cgroup controllers, the Landlock ABI, and a prepared rootfs directory.
pub struct HostEnvironmentProbe;

impl EnvironmentProbe for HostEnvironmentProbe {
    fn detect(&self) -> Capabilities {
        let path_var = env::var_os("PATH").unwrap_or_default();
        let user_ns = user_namespaces_available();
        Capabilities {
            user_ns,
            pasta: find_on_path("pasta", &path_var),
            ip: find_on_path("ip", &path_var),
            cgroup: delegated_controllers(&read_cgroup_controllers()),
            landlock_abi: landlock_abi(),
            overlayfs_rootless: overlayfs_rootless(&read_or_empty(PROC_FILESYSTEMS), user_ns),
            notify_send: find_on_path("notify-send", &path_var),
            git: find_on_path("git", &path_var).is_some(),
        }
    }

    fn inspect_rootfs(&self, path: &Path, shell: Option<&str>) -> RootfsFacts {
        RootfsFacts {
            path: path.to_path_buf(),
            exists: path.is_dir(),
            has_default_shell: resolve_in_rootfs(path, DEFAULT_SHELL),
            configured_shell: shell.map(|shell| ConfiguredShell {
                path: shell.to_owned(),
                present: resolve_in_rootfs(path, shell),
            }),
            workdir_writable: workdir_writable(path),
        }
    }

    fn inspect_mount_sources(&self, paths: &[PathBuf]) -> Vec<MountSourceFacts> {
        // `exists` and not `try_exists`: a path hort is refused permission to
        // read has to answer absent and be skipped, because the alternative is
        // handing the runtime a bind it cannot take and losing the whole box.
        paths
            .iter()
            .map(|path| MountSourceFacts { path: path.clone(), exists: path.exists() })
            .collect()
    }
}

/// The first entry of `path_var` holding an executable file named `program`.
fn find_on_path(program: &str, path_var: &OsStr) -> Option<PathBuf> {
    env::split_paths(path_var).map(|entry| entry.join(program)).find(|candidate| {
        fs::metadata(candidate)
            .is_ok_and(|entry| entry.is_file() && entry.permissions().mode() & EXECUTE_BITS != 0)
    })
}

/// Which of the controllers hort cares about a `cgroup.controllers` line lists.
fn delegated_controllers(controllers: &str) -> CgroupCaps {
    // Whole tokens, never substrings: a subtree can delegate `cpuset` without
    // `cpu`, and reading one as the other would cap a controller that is not
    // there, which is the silent degradation the capability report exists to
    // make loud.
    let delegated = |controller| controllers.split_whitespace().any(|token| token == controller);
    CgroupCaps {
        memory: delegated("memory"),
        pids: delegated("pids"),
        cpu: delegated("cpu"),
        cpuset: delegated("cpuset"),
    }
}

/// Whether this host can be expected to mount an overlay without privileges.
fn overlayfs_rootless(filesystems: &str, user_ns: bool) -> bool {
    user_ns && filesystems.split_whitespace().any(|filesystem| filesystem == "overlay")
}

/// Whether `target` resolves to a file inside `rootfs`, resolving each component
/// from the rootfs root: an absolute link target is re-anchored at the rootfs and
/// a relative one resolves against the link's own directory.
fn resolve_in_rootfs(rootfs: &Path, target: &str) -> bool {
    let mut hops = 0;
    resolve_within(rootfs, rootfs, Path::new(target), &mut hops)
        .and_then(|resolved| fs::symlink_metadata(resolved).ok())
        .is_some_and(|resolved| resolved.is_file())
}

/// Walk `target` from `base`, one component at a time, and answer where it lands
/// inside `rootfs`. Every symlink is expanded here rather than by the host, so a
/// link that points outside the rootfs lands where the sandbox would find it, not
/// where this process would.
fn resolve_within(rootfs: &Path, base: &Path, target: &Path, hops: &mut usize) -> Option<PathBuf> {
    let mut resolved = base.to_path_buf();
    for component in target.components() {
        match component {
            Component::RootDir => resolved = rootfs.to_path_buf(),
            Component::ParentDir => {
                if resolved != rootfs {
                    resolved.pop();
                }
            }
            Component::CurDir | Component::Prefix(_) => {}
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                let entry = fs::symlink_metadata(&candidate).ok()?;
                if !entry.file_type().is_symlink() {
                    resolved = candidate;
                    continue;
                }
                if *hops == MAX_LINK_HOPS {
                    return None;
                }
                *hops += 1;
                let link = fs::read_link(&candidate).ok()?;
                resolved = resolve_within(rootfs, &resolved, &link, hops)?;
            }
        }
    }
    Some(resolved)
}

/// Whether the rootfs offers a directory to bind the worktree onto. Strict on
/// purpose: the mount destination is what the sandbox's `/workdir` becomes, and a
/// rootfs shipping it as a file breaks the bind outright.
fn workdir_writable(rootfs: &Path) -> bool {
    fs::metadata(rootfs.join(WORKDIR))
        .is_ok_and(|workdir| workdir.is_dir() && workdir.permissions().mode() & WORLD_WRITE != 0)
}

/// Whether this kernel grants an unprivileged user namespace, observed by asking
/// for one in a forked child.
fn user_namespaces_available() -> bool {
    match unsafe { libc::fork() } {
        -1 => false,
        // Only the syscall and the exit run on this side: after a fork the child
        // may touch nothing that allocates.
        0 => {
            let unshared = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
            unsafe { libc::_exit(if unshared == 0 { 0 } else { 1 }) }
        }
        child => {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(child, &mut status, 0) };
            waited == child && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
        }
    }
}

/// The Landlock ABI version this kernel supports, or `None` where Landlock is
/// absent or disabled.
fn landlock_abi() -> Option<u8> {
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<libc::c_void>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    u8::try_from(version).ok().filter(|abi| *abi >= 1)
}

/// The `cgroup.controllers` of hort's own cgroup. A host that does not answer
/// reads as no controller delegated.
fn read_cgroup_controllers() -> String {
    let Some(cgroup) = own_cgroup_path() else {
        return String::new();
    };
    let mount = cgroup2_mount_point().unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP2_MOUNT));
    read_or_empty(mount.join(cgroup).join(CONTROLLERS_FILE))
}

/// Where hort's own cgroup sits under the cgroup2 mount, taken from the `0::`
/// line that is the whole of `/proc/self/cgroup` on a v2 host.
fn own_cgroup_path() -> Option<String> {
    let cgroups = fs::read_to_string(PROC_SELF_CGROUP).ok()?;
    let path = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(path.trim_start_matches('/').to_owned())
}

/// Where cgroup2 is mounted, found in the mount table by filesystem type.
fn cgroup2_mount_point() -> Option<PathBuf> {
    let mounts = fs::read_to_string(PROC_MOUNTS).ok()?;
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let point = fields.nth(1)?;
        (fields.next() == Some(CGROUP2_FSTYPE)).then(|| PathBuf::from(point))
    })
}

fn read_or_empty(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::TempDir;

    /// A `/proc/filesystems` listing in the kernel's own format: one filesystem
    /// per line, prefixed with `nodev` when it needs no block device.
    const FILESYSTEMS_WITH_OVERLAY: &str = "nodev\tsysfs\nnodev\ttmpfs\n\text4\nnodev\toverlay\n";
    const FILESYSTEMS_WITHOUT_OVERLAY: &str = "nodev\tsysfs\nnodev\ttmpfs\n\text4\n";

    /// A scratch directory to build a rootfs or a `PATH` entry in.
    fn temp_dir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    /// Create an empty file at `path` with `mode`, creating the directories above
    /// it.
    fn write_file(path: &Path, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Create a directory at `path` with `mode`.
    fn make_dir(path: &Path, mode: u32) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn find_on_path_returns_the_first_executable_match() {
        let (_first, first) = temp_dir();
        let (_second, second) = temp_dir();
        write_file(&first.join("pasta"), 0o755);
        write_file(&second.join("pasta"), 0o755);
        let path_var = OsString::from(format!("{}:{}", first.display(), second.display()));

        let found = find_on_path("pasta", &path_var);

        assert_eq!(found, Some(first.join("pasta")));
    }

    #[test]
    fn find_on_path_skips_a_directory_named_like_the_program() {
        let (_first, first) = temp_dir();
        let (_second, second) = temp_dir();
        // A directory carries execute bits of its own, so only "is a file" tells
        // it apart from the program being looked for.
        make_dir(&first.join("pasta"), 0o755);
        write_file(&second.join("pasta"), 0o755);
        let path_var = OsString::from(format!("{}:{}", first.display(), second.display()));

        let found = find_on_path("pasta", &path_var);

        assert_eq!(found, Some(second.join("pasta")));
    }

    #[test]
    fn find_on_path_ignores_a_file_without_an_execute_bit() {
        let (_dir, dir) = temp_dir();
        write_file(&dir.join("pasta"), 0o644);

        let found = find_on_path("pasta", dir.as_os_str());

        assert_eq!(found, None);
    }

    #[test]
    fn delegated_controllers_maps_a_delegated_line_to_the_capability_set() {
        let controllers = delegated_controllers("cpu io memory pids\n");

        assert!(controllers.cpu);
        assert!(controllers.memory);
        assert!(controllers.pids);
        assert!(!controllers.cpuset);
    }

    #[test]
    fn delegated_controllers_reports_every_controller_absent_for_an_empty_line() {
        let controllers = delegated_controllers("");

        assert!(!controllers.cpu);
        assert!(!controllers.memory);
        assert!(!controllers.pids);
        assert!(!controllers.cpuset);
    }

    #[test]
    fn delegated_controllers_does_not_read_cpuset_as_cpu() {
        // A subtree can delegate cpuset without cpu, and "cpuset" carries "cpu"
        // as a substring, so only a token-wise reading tells the two apart.
        let controllers = delegated_controllers("cpuset io memory pids\n");

        assert!(!controllers.cpu);
        assert!(controllers.cpuset);
    }

    #[test]
    fn overlayfs_rootless_is_reported_when_the_kernel_lists_overlay_and_user_namespaces_work() {
        assert!(overlayfs_rootless(FILESYSTEMS_WITH_OVERLAY, true));
    }

    #[test]
    fn overlayfs_rootless_is_absent_when_the_kernel_does_not_list_overlay() {
        assert!(!overlayfs_rootless(FILESYSTEMS_WITHOUT_OVERLAY, true));
    }

    #[test]
    fn overlayfs_rootless_is_absent_without_user_namespaces() {
        assert!(!overlayfs_rootless(FILESYSTEMS_WITH_OVERLAY, false));
    }

    #[test]
    fn inspect_rootfs_reports_a_prepared_rootfs_as_ready() {
        let (_dir, rootfs) = temp_dir();
        write_file(&rootfs.join("bin").join("sh"), 0o755);
        make_dir(&rootfs.join("workdir"), 0o1777);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert_eq!(facts.path, rootfs);
        assert!(facts.exists);
        assert!(facts.has_default_shell);
        assert!(facts.configured_shell.is_none());
        assert!(facts.workdir_writable);
    }

    #[test]
    fn inspect_rootfs_reports_a_missing_directory_as_absent() {
        let (_dir, dir) = temp_dir();

        let facts = HostEnvironmentProbe.inspect_rootfs(&dir.join("nowhere"), None);

        assert!(!facts.exists);
    }

    #[test]
    fn inspect_rootfs_reports_a_file_path_as_absent() {
        let (_dir, dir) = temp_dir();
        let rootfs = dir.join("rootfs");
        write_file(&rootfs, 0o644);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(!facts.exists);
    }

    #[test]
    fn inspect_rootfs_follows_an_absolute_shell_symlink_inside_the_rootfs() {
        let (_dir, rootfs) = temp_dir();
        // What a real export ships: the link target is absolute and names a path
        // that only exists inside the rootfs.
        write_file(&rootfs.join("bin").join("busybox"), 0o755);
        symlink("/bin/busybox", rootfs.join("bin").join("sh")).unwrap();

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(facts.has_default_shell);
    }

    #[test]
    fn inspect_rootfs_follows_an_absolute_symlink_in_an_intermediate_component() {
        let (_dir, rootfs) = temp_dir();
        // The merged-usr layout, where /bin is itself a link.
        write_file(&rootfs.join("usr").join("bin").join("sh"), 0o755);
        symlink("/usr/bin", rootfs.join("bin")).unwrap();

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(facts.has_default_shell);
    }

    #[test]
    fn inspect_rootfs_reports_no_default_shell_for_a_dangling_symlink() {
        let (_dir, rootfs) = temp_dir();
        // The target is a path most hosts carry, so resolving through the host
        // instead of through the rootfs would call this shell present.
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        symlink("/bin/bash", rootfs.join("bin").join("sh")).unwrap();

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(!facts.has_default_shell);
    }

    #[test]
    fn inspect_rootfs_reports_the_configured_shell_as_present() {
        let (_dir, rootfs) = temp_dir();
        write_file(&rootfs.join("bin").join("zsh"), 0o755);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, Some("/bin/zsh"));

        let configured = facts.configured_shell.expect("the configuration declared a shell");
        assert_eq!(configured.path, "/bin/zsh");
        assert!(configured.present);
    }

    #[test]
    fn inspect_rootfs_reports_the_configured_shell_as_absent() {
        let (_dir, rootfs) = temp_dir();
        write_file(&rootfs.join("bin").join("sh"), 0o755);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, Some("/bin/zsh"));

        let configured = facts.configured_shell.expect("the configuration declared a shell");
        assert!(!configured.present);
    }

    #[test]
    fn inspect_rootfs_reports_workdir_not_writable_without_the_directory() {
        let (_dir, rootfs) = temp_dir();
        write_file(&rootfs.join("bin").join("sh"), 0o755);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(!facts.workdir_writable);
    }

    #[test]
    fn inspect_rootfs_reports_workdir_not_writable_for_a_non_world_writable_directory() {
        let (_dir, rootfs) = temp_dir();
        make_dir(&rootfs.join("workdir"), 0o755);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(!facts.workdir_writable);
    }

    #[test]
    fn inspect_rootfs_reports_workdir_not_writable_when_it_is_a_file() {
        let (_dir, rootfs) = temp_dir();
        // World-writable, and still no place to bind the worktree onto.
        write_file(&rootfs.join("workdir"), 0o666);

        let facts = HostEnvironmentProbe.inspect_rootfs(&rootfs, None);

        assert!(!facts.workdir_writable);
    }

    #[test]
    fn inspect_mount_sources_answers_one_fact_per_path_in_the_order_asked() {
        let (_dir, dir) = temp_dir();
        let present = dir.join(".config").join("fish");
        make_dir(&present, 0o755);
        let absent = dir.join(".tmux.conf");

        let facts = HostEnvironmentProbe.inspect_mount_sources(&[present.clone(), absent.clone()]);

        // Nothing above this layer asks the disk anything, so this is where the
        // feature is decided: a probe that answered short, or out of order, or
        // absent for what is there, builds a box carrying none of the paths the
        // user declared and reports no error doing it.
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].path, present);
        assert!(facts[0].exists);
        assert_eq!(facts[1].path, absent);
        assert!(!facts[1].exists);
    }

    #[test]
    fn detect_reports_git_on_a_host_that_can_run_this_suite() {
        let capabilities = HostEnvironmentProbe.detect();

        assert!(capabilities.git);
    }

    #[test]
    fn detect_reports_ip_on_a_host_that_can_run_this_suite() {
        let capabilities = HostEnvironmentProbe.detect();

        assert!(capabilities.ip.is_some());
    }
}
