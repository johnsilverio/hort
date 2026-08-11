//! Which host paths a sandbox carries, and where the box finds them. A pure
//! decision over plain data: the configuration says what to mount, the host says
//! what is actually there, and this decides the plan.

use std::path::{Path, PathBuf};

use crate::domain::config::{CacheDir, ResolvedConfig};
use crate::domain::error::HortError;
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

/// A planned cache whose target lands inside a planned read-only mount, and the
/// host path its mountpoint would have to be created at. It carries what the
/// refusal names alongside the path the host is asked about, so the two cannot
/// be paired wrong later.
pub struct CacheLanding {
    pub name: String,
    pub target: PathBuf,
    pub source: PathBuf,
    pub host_path: PathBuf,
}

/// Where each planned cache lands inside a planned read-only mount. Reads the
/// two plans and never the configuration, so a read-only mount the host does
/// not have, and which therefore never made it into the plan, cannot make a
/// cache illegal.
pub fn cache_landings(read_only: &[SandboxMount], caches: &[SandboxMount]) -> Vec<CacheLanding> {
    caches
        .iter()
        .filter_map(|cache| {
            let (covering, suffix) = covering_mount(read_only, &cache.target)?;
            Some(CacheLanding {
                name: entry_name(&cache.source),
                target: cache.target.clone(),
                source: covering.source.clone(),
                host_path: landing_path(&covering.source, suffix),
            })
        })
        .collect()
}

/// The refusal a cache earns when the host does not have the path its
/// mountpoint would be created at, given what the host said about the landings.
pub fn cache_landing_error(
    landings: &[CacheLanding],
    sources: &[MountSourceFacts],
) -> Option<HortError> {
    let missing = landings.iter().find(|landing| !host_has(sources, &landing.host_path))?;
    Some(HortError::CacheTargetMissing {
        name: missing.name.clone(),
        target: missing.target.display().to_string(),
        source: missing.source.display().to_string(),
    })
}

/// The read-only mount a cache aimed at `target` lands inside, and how deep it
/// lands, or nothing when no read-only mount covers it. Of several that cover
/// one target the last is the answer: the mounts are applied in order, so the
/// later one is what the box shows at that path and therefore the tree the
/// mountpoint would have to be created in.
fn covering_mount<'a>(
    read_only: &'a [SandboxMount],
    target: &'a Path,
) -> Option<(&'a SandboxMount, &'a Path)> {
    read_only.iter().rev().find_map(|mount| Some((mount, target.strip_prefix(&mount.target).ok()?)))
}

/// Where a cache landing sits on the host: the source of the mount covering it,
/// walked down by how deep the cache lands inside that mount.
fn landing_path(source: &Path, depth: &Path) -> PathBuf {
    // Joining a cache that covers the whole mount, and so lands no deeper than
    // it, leaves a trailing separator, and a path spelled that way is one the
    // host reports absent unless it is a directory.
    match depth.as_os_str().is_empty() {
        true => source.to_path_buf(),
        false => source.join(depth),
    }
}

/// Whether the host has a landing's path, read from the fact carrying that same
/// path. A path nothing answered for counts as absent, so an answer hort cannot
/// pair up refuses the build rather than letting it through to the kernel.
fn host_has(sources: &[MountSourceFacts], path: &Path) -> bool {
    sources.iter().find(|fact| fact.path == path).is_some_and(|fact| fact.exists)
}

/// What the user called a cache, read back from the directory hort keeps it in,
/// which is the entry's own name under this project's cache root.
fn entry_name(source: &Path) -> String {
    source.file_name().unwrap_or_default().to_string_lossy().into_owned()
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

    /// The host paths these landings need, which is everything the check asks
    /// the host about.
    fn landing_paths(landings: &[CacheLanding]) -> Vec<PathBuf> {
        landings.iter().map(|landing| landing.host_path.clone()).collect()
    }

    /// A host that has every path the landings need.
    fn every_path_present(landings: &[CacheLanding]) -> Vec<MountSourceFacts> {
        landings
            .iter()
            .map(|landing| MountSourceFacts { path: landing.host_path.clone(), exists: true })
            .collect()
    }

    /// A host that has none of them.
    fn every_path_absent(landings: &[CacheLanding]) -> Vec<MountSourceFacts> {
        landings
            .iter()
            .map(|landing| MountSourceFacts { path: landing.host_path.clone(), exists: false })
            .collect()
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
    fn a_cache_inside_a_read_only_mount_is_checked_against_the_host_path_behind_it() {
        let read_only = vec![read_only_mount("/home/tester/.config", "/home/hort/.config")];
        let caches = vec![writable_mount(&format!("{CACHE_ROOT}/fish"), "/home/hort/.config/fish")];

        let landings = cache_landings(&read_only, &caches);

        // What the box shows at that target is the read-only source, so the
        // directory the cache needs to be mounted over is the one behind it on
        // the host. Asking about anything else asks about a path the mount
        // never touches.
        assert_eq!(landing_paths(&landings), vec![PathBuf::from("/home/tester/.config/fish")]);
    }

    #[test]
    fn a_cache_outside_every_read_only_mount_asks_the_host_nothing() {
        let read_only = vec![read_only_mount("/home/tester/.config", "/home/hort/.config")];
        let caches =
            vec![writable_mount(&format!("{CACHE_ROOT}/node_modules"), "/workdir/node_modules")];

        let landings = cache_landings(&read_only, &caches);

        // The common cache sits in the worktree, which is a writable bind of its
        // own: nothing is read-only there, and a question asked about it could
        // only ever refuse a configuration that works.
        assert!(landing_paths(&landings).is_empty());
    }

    #[test]
    fn a_cache_whose_path_is_missing_inside_the_read_only_mount_is_refused() {
        let read_only = vec![read_only_mount("/home/tester/.config", "/home/hort/.config")];
        let caches = vec![writable_mount(&format!("{CACHE_ROOT}/fish"), "/home/hort/.config/fish")];
        let landings = cache_landings(&read_only, &caches);

        let refusal = cache_landing_error(&landings, &every_path_absent(&landings));

        // Mounting there means creating the mountpoint inside a tree the same
        // user declared read-only, which cannot be done, and the container dies
        // naming the rootfs, the one piece that has nothing to do with it.
        // Dropping the cache instead would be invisible from inside the box: the
        // dependency reinstalls every run and nothing anywhere says why.
        assert_eq!(
            refusal,
            Some(HortError::CacheTargetMissing {
                name: "fish".to_string(),
                target: "/home/hort/.config/fish".to_string(),
                source: "/home/tester/.config".to_string(),
            })
        );
    }

    #[test]
    fn a_cache_whose_path_exists_inside_the_read_only_mount_is_permitted() {
        let read_only = vec![read_only_mount("/home/tester/.config", "/home/hort/.config")];
        let caches = vec![writable_mount(&format!("{CACHE_ROOT}/fish"), "/home/hort/.config/fish")];
        let landings = cache_landings(&read_only, &caches);

        let refusal = cache_landing_error(&landings, &every_path_present(&landings));

        // Measured, and it is what keeps this rule narrow: with the directory
        // there the box boots and the cache is a writable island inside the
        // read-only tree, because the mounts are applied in order and the cache
        // wins at that path. A rule that refused every cache inside a read-only
        // mount would refuse this.
        assert_eq!(refusal, None);
    }

    #[test]
    fn a_cache_that_shadows_a_read_only_mount_entirely_is_permitted() {
        let read_only = vec![read_only_mount("/home/tester/.cache/pip", "/home/hort/.cache/pip")];
        let caches = vec![writable_mount(&format!("{CACHE_ROOT}/pip"), "/home/hort/.cache/pip")];
        let landings = cache_landings(&read_only, &caches);

        let refusal = cache_landing_error(&landings, &every_path_present(&landings));

        // Two declarations of the same address, where the writable one wins and
        // covers the read-only one whole. There is no path to create inside
        // anything: what the box needs is the read-only source itself, and hort
        // is mounting it, so it is there.
        assert_eq!(refusal, None);
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
