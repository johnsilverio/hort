//! Which host paths a sandbox carries, and where the box finds them. A pure
//! decision over plain data: the configuration says what to mount, the host says
//! what is actually there, and this decides the plan.

use std::path::{Path, PathBuf};

use crate::domain::config::ResolvedConfig;
use crate::domain::model::Warning;
use crate::ports::SandboxMount;

/// The home every process in the sandbox runs with. Fixed rather than
/// configurable: it is the address a carried-over dotfile is re-anchored at, and
/// the same path the sandbox exports as `HOME`.
pub const SANDBOX_HOME: &str = "/home/hort";

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
        };
        if !mounts.contains(&mount) {
            mounts.push(mount);
        }
    }
    (mounts, warnings)
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::config::{Agent, Auth, Cache, Mounts, Notify};

    /// The home the user has on the host in these tests. A literal, never a
    /// scratch directory: nothing here touches the disk, the home is only ever
    /// the anchor a declared path is measured against.
    const HOST_HOME: &str = "/home/tester";

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

    /// The mounts `config` asks for, planned the way `up` plans them on a host
    /// that has every declared source.
    fn plan_of(config: &ResolvedConfig) -> (Vec<SandboxMount>, Vec<Warning>) {
        let present: Vec<MountSourceFacts> = declared_read_only_sources(config)
            .into_iter()
            .map(|path| MountSourceFacts { path, exists: true })
            .collect();
        read_only_mount_plan(&present, Path::new(HOST_HOME))
    }

    fn mount(source: &str, target: &str) -> SandboxMount {
        SandboxMount { source: PathBuf::from(source), target: PathBuf::from(target) }
    }

    #[test]
    fn read_only_mount_lands_inside_the_sandbox_home() {
        let config = config_declaring(&["/home/tester/.config/fish"], &[]);

        let (mounts, _) = plan_of(&config);

        // A tool finds its configuration under the home it is running with, and
        // inside the box that is the home hort gives the sandbox. Carried over
        // at the host path, the user's own shell configuration would sit
        // somewhere no shell in the box ever looks.
        assert_eq!(mounts, vec![mount("/home/tester/.config/fish", "/home/hort/.config/fish")]);
    }

    #[test]
    fn a_source_outside_the_host_home_keeps_its_absolute_path() {
        let config = config_declaring(&["/etc/hort/shared.conf"], &[]);

        let (mounts, _) = plan_of(&config);

        // Nothing outside the user's home is a dotfile, so there is no suffix to
        // re-anchor: where the tool that reads it looks is the absolute path it
        // was declared under, and moving it under the sandbox home would hide it
        // from the only thing that wanted it.
        assert_eq!(mounts, vec![mount("/etc/hort/shared.conf", "/etc/hort/shared.conf")]);
    }

    #[test]
    fn an_agent_credential_lands_inside_the_sandbox_home() {
        let config = config_declaring(&[], &["/home/tester/.claude"]);

        let (mounts, _) = plan_of(&config);

        // Credentials are declared per agent rather than under the mount list,
        // and they are the half of this that decides whether an agent starts
        // logged in or asks the user to log in again inside a box that dies.
        assert_eq!(mounts, vec![mount("/home/tester/.claude", "/home/hort/.claude")]);
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
        assert_eq!(mounts, vec![mount("/home/tester/.config/fish", "/home/hort/.config/fish")]);
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
        assert_eq!(mounts, vec![mount("/home/tester/.claude", "/home/hort/.claude")]);
    }
}
