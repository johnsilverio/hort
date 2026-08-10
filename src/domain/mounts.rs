//! Which host paths a sandbox carries, and where the box finds them. A pure
//! decision over plain data: the configuration says what to mount, the host says
//! what is actually there, and this decides the plan.

use std::path::{Path, PathBuf};

use crate::domain::config::{CacheDir, ResolvedConfig};
use crate::domain::model::Warning;
use crate::ports::{MountAccess, SandboxMount};

/// The home every process in the sandbox runs with. Fixed rather than
/// configurable: it is the address a carried-over dotfile is re-anchored at, and
/// the same path the sandbox exports as `HOME`.
pub const SANDBOX_HOME: &str = "/home/hort";

/// Where the box finds the worktree, and therefore what a cache declared by a
/// bare name is a name inside of.
pub const WORKDIR: &str = "/workdir";

/// What the host says about one declared mount source. It carries its own path,
/// so a source that was declared but never inspected cannot be represented.
pub struct MountSourceFacts {
    pub path: PathBuf,
    pub exists: bool,
}

/// Every host path the configuration declares read-only, in the order the
/// sandbox mounts them: the dotfiles first, then the credentials of each agent.
pub fn declared_read_only_sources(config: &ResolvedConfig) -> Vec<PathBuf> {
    config
        .mounts
        .read_only
        .iter()
        .chain(config.agents.iter().flat_map(|agent| agent.auth.read_only.iter()))
        .map(PathBuf::from)
        .collect()
}

/// Plan the read-only mounts of the sources the host was asked about, against
/// the home the user has on `host_home`.
pub fn read_only_mount_plan(
    sources: &[MountSourceFacts],
    host_home: &Path,
) -> (Vec<SandboxMount>, Vec<Warning>) {
    let mut mounts: Vec<SandboxMount> = Vec::new();
    let mut warnings = Vec::new();
    for source in sources {
        if !source.exists {
            warnings.push(Warning::new(format!(
                "read-only mount '{}' is not on this host, so the sandbox starts without it",
                source.path.display()
            )));
            continue;
        }
        let mount = SandboxMount {
            source: source.path.clone(),
            target: sandbox_target(&source.path, host_home),
            access: MountAccess::ReadOnly,
        };
        if !mounts.contains(&mount) {
            mounts.push(mount);
        }
    }
    (mounts, warnings)
}

/// Plan the writable cache mounts the configuration declares, with `cache_root`
/// the directory this project's caches live under on the host.
pub fn cache_mount_plan(config: &ResolvedConfig, cache_root: &Path) -> Vec<SandboxMount> {
    config
        .cache
        .dirs
        .iter()
        .map(|dir| {
            let (name, target) = match dir {
                CacheDir::Path(name) => (name, Path::new(WORKDIR).join(name)),
                CacheDir::Named { name, target } => (name, sandbox_path(target)),
            };
            SandboxMount { source: cache_root.join(name), target, access: MountAccess::ReadWrite }
        })
        .collect()
}

/// Where the box finds a host path. A path under the user's own home is a
/// dotfile, and a tool inside the box looks for it under the home that tool is
/// running with; anything else keeps its absolute path, which is the only place
/// whatever reads it ever looks.
fn sandbox_target(source: &Path, host_home: &Path) -> PathBuf {
    match source.strip_prefix(host_home) {
        Ok(suffix) => Path::new(SANDBOX_HOME).join(suffix),
        Err(_) => source.to_path_buf(),
    }
}

/// A path the user wrote for the inside of the box, with the home shorthand
/// resolved. The configuration reader leaves it alone in a container path on
/// purpose, since the home it stands for is the sandbox's and not the host's.
fn sandbox_path(declared: &str) -> PathBuf {
    match declared.strip_prefix("~/") {
        Some(tail) => Path::new(SANDBOX_HOME).join(tail),
        None => PathBuf::from(declared),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::config::{Agent, Auth, Cache, CacheDir, Mounts, Notify};

    /// The home the user has on the host in these tests. A literal, never a
    /// scratch directory: nothing here touches the disk, the home is only ever
    /// the anchor a declared path is measured against.
    const HOST_HOME: &str = "/home/tester";

    /// Where one project's caches sit on the host: hort's own state directory,
    /// then the one directory name that stands for the project.
    const CACHE_ROOT: &str = "/state/cache/%2Fhome%2Ftester%2Fprojects%2Fhort";

    /// A configuration declaring `read_only` under `mounts.readOnly` and
    /// `credentials` under the one agent's `auth.readOnly`, with every path
    /// already expanded the way the configuration reader hands them over.
    fn config_declaring(read_only: &[&str], credentials: &[&str]) -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some("/base/rootfs".to_string()),
            agents: vec![Agent {
                command: "claude".to_string(),
                auth: Auth {
                    read_only: credentials.iter().map(|path| (*path).to_string()).collect(),
                    env: Vec::new(),
                },
                notify: None::<Notify>,
            }],
            mounts: Mounts {
                read_only: read_only.iter().map(|path| (*path).to_string()).collect(),
            },
            network: Vec::new(),
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: None,
            resources: None,
        }
    }

    /// A configuration declaring `dirs` under `cache.dirs` and nothing else.
    fn config_caching(dirs: Vec<CacheDir>) -> ResolvedConfig {
        ResolvedConfig { cache: Cache { dirs }, ..config_declaring(&[], &[]) }
    }

    /// The mounts `config` asks for, planned the way `up` plans them on a host
    /// that has every declared source.
    fn plan_of(config: &ResolvedConfig) -> (Vec<SandboxMount>, Vec<Warning>) {
        let present: Vec<MountSourceFacts> = declared_read_only_sources(config)
            .into_iter()
            .map(|path| MountSourceFacts { path, exists: true })
            .collect();
        read_only_mount_plan(&present, Path::new(HOST_HOME))
    }

    fn read_only_mount(source: &str, target: &str) -> SandboxMount {
        SandboxMount {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            access: MountAccess::ReadOnly,
        }
    }

    fn writable_mount(source: &str, target: &str) -> SandboxMount {
        SandboxMount {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            access: MountAccess::ReadWrite,
        }
    }

    #[test]
    fn read_only_mount_lands_inside_the_sandbox_home() {
        let config = config_declaring(&["/home/tester/.config/fish"], &[]);

        let (mounts, _) = plan_of(&config);

        // A tool finds its configuration under the home it is running with, and
        // inside the box that is the home hort gives the sandbox. Carried over
        // at the host path, the user's own shell configuration would sit
        // somewhere no shell in the box ever looks.
        assert_eq!(
            mounts,
            vec![read_only_mount("/home/tester/.config/fish", "/home/hort/.config/fish")]
        );
    }

    #[test]
    fn a_source_outside_the_host_home_keeps_its_absolute_path() {
        let config = config_declaring(&["/etc/hort/shared.conf"], &[]);

        let (mounts, _) = plan_of(&config);

        // Nothing outside the user's home is a dotfile, so there is no suffix to
        // re-anchor: where the tool that reads it looks is the absolute path it
        // was declared under, and moving it under the sandbox home would hide it
        // from the only thing that wanted it.
        assert_eq!(mounts, vec![read_only_mount("/etc/hort/shared.conf", "/etc/hort/shared.conf")]);
    }

    #[test]
    fn an_agent_credential_lands_inside_the_sandbox_home() {
        let config = config_declaring(&[], &["/home/tester/.claude"]);

        let (mounts, _) = plan_of(&config);

        // Credentials are declared per agent rather than under the mount list,
        // and they are the half of this that decides whether an agent starts
        // logged in or asks the user to log in again inside a box that dies.
        assert_eq!(mounts, vec![read_only_mount("/home/tester/.claude", "/home/hort/.claude")]);
    }

    #[test]
    fn a_missing_source_is_not_mounted() {
        let sources = vec![
            MountSourceFacts { path: PathBuf::from("/home/tester/.config/fish"), exists: true },
            MountSourceFacts { path: PathBuf::from("/home/tester/.tmux.conf"), exists: false },
        ];

        let (mounts, _) = read_only_mount_plan(&sources, Path::new(HOST_HOME));

        // A bind whose source is not there takes the whole container down, and
        // the paths in this list are the user's: hort did not invent them and
        // cannot know what should have been there. A dotfile the user stopped
        // keeping must not be what stops the box from booting.
        assert_eq!(
            mounts,
            vec![read_only_mount("/home/tester/.config/fish", "/home/hort/.config/fish")]
        );
    }

    #[test]
    fn a_missing_source_is_reported_as_a_warning() {
        let sources = vec![MountSourceFacts {
            path: PathBuf::from("/home/tester/.tmux.conf"),
            exists: false,
        }];

        let (_, warnings) = read_only_mount_plan(&sources, Path::new(HOST_HOME));

        // Degrading quietly leaves the user inside a box missing the tools they
        // declared, with nothing anywhere saying which one or why.
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].to_string().contains("/home/tester/.tmux.conf"));
    }

    #[test]
    fn a_path_declared_in_both_families_is_mounted_once() {
        let config = config_declaring(&["/home/tester/.claude"], &["/home/tester/.claude"]);

        let (mounts, _) = plan_of(&config);

        // The same directory under both keys is the reachable collision: a
        // credential the user also lists among the paths they want carried over.
        // Both entries name one source and one destination, so the second is the
        // first said twice.
        assert_eq!(mounts, vec![read_only_mount("/home/tester/.claude", "/home/hort/.claude")]);
    }

    #[test]
    fn a_bare_cache_entry_mounts_inside_the_worktree() {
        let config = config_caching(vec![CacheDir::Path("node_modules".to_string())]);

        let mounts = cache_mount_plan(&config, Path::new(CACHE_ROOT));

        // The common case is a directory a package manager fills in the project
        // it belongs to, so a bare name is read as a name inside the worktree.
        // Writable is the whole point: a cache the box cannot write to costs the
        // same four minutes on every boot that not having one does.
        assert_eq!(
            mounts,
            vec![writable_mount(
                "/state/cache/%2Fhome%2Ftester%2Fprojects%2Fhort/node_modules",
                "/workdir/node_modules"
            )]
        );
    }

    #[test]
    fn a_named_cache_entry_mounts_at_its_declared_target() {
        let config = config_caching(vec![CacheDir::Named {
            name: "pip".to_string(),
            target: "/var/cache/pip".to_string(),
        }]);

        let mounts = cache_mount_plan(&config, Path::new(CACHE_ROOT));

        // Plenty of tools keep their cache somewhere the project never sees, so
        // the object form exists to say where. The host side stays hort's own
        // either way: the entry names the address inside the box, never outside.
        assert_eq!(
            mounts,
            vec![writable_mount(
                "/state/cache/%2Fhome%2Ftester%2Fprojects%2Fhort/pip",
                "/var/cache/pip"
            )]
        );
    }

    #[test]
    fn a_cache_target_shorthand_expands_against_the_sandbox_home() {
        let config = config_caching(vec![CacheDir::Named {
            name: "pip".to_string(),
            target: "~/.cache/pip".to_string(),
        }]);

        let mounts = cache_mount_plan(&config, Path::new(CACHE_ROOT));

        // A cache target names a directory inside the box, and the home it is
        // shorthand for is the sandbox's. Nothing before this resolves it: the
        // configuration reader expands the shorthand in host paths only, on
        // purpose, because expanding a container path against the host's home
        // would silently address a directory on the wrong side of the box.
        assert_eq!(
            mounts,
            vec![writable_mount(
                "/state/cache/%2Fhome%2Ftester%2Fprojects%2Fhort/pip",
                "/home/hort/.cache/pip"
            )]
        );
    }
}
