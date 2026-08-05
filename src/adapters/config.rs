//! `ConfigResolver`: the reader that turns the configuration layers on disk into
//! one resolved configuration. It locates the global file under the config root
//! and the project-local file at the project directory, reads them, expands the
//! home shorthand in the host paths, and merges them with the local layer
//! winning.
//!
//! Parsing and merging themselves are pure decisions and live in the domain; this
//! module owns only the file locating, the reading, and the expansion. A layer
//! that is absent is not an error: a user with no global file and a project with
//! no configuration both resolve to defaults.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::config::{Config, ResolvedConfig, map_devcontainer, merge, parse};
use crate::domain::error::HortError;
use crate::domain::model::Warning;

const GLOBAL_FILE: &str = "config.json";
const LOCAL_FILE: &str = ".hort.json";
const DEVCONTAINER_DIR: &str = ".devcontainer";
const DEVCONTAINER_FILE: &str = "devcontainer.json";

/// Reads and merges the configuration layers rooted at a config directory, a
/// project directory, and the user's home.
///
/// The three roots arrive through the constructor rather than being read from
/// the environment, so callers stay explicit about where configuration comes
/// from. The home is separate from the config root because the config root can
/// point outside the home.
pub struct ConfigResolver {
    config_root: PathBuf,
    project_dir: PathBuf,
    home: PathBuf,
}

impl ConfigResolver {
    /// Build a resolver over the global config directory, the project directory,
    /// and the home the shorthand expands to.
    pub fn new(config_root: PathBuf, project_dir: PathBuf, home: PathBuf) -> Self {
        Self { config_root, project_dir, home }
    }

    /// Resolve the configuration for this project: the global layer merged with
    /// the project layer, local winning, plus any advisories raised while
    /// mapping a `devcontainer.json` fallback.
    ///
    /// A missing layer contributes defaults. A layer that exists but cannot be
    /// read or parsed is an error naming the file.
    pub fn resolve(&self) -> Result<(ResolvedConfig, Vec<Warning>), HortError> {
        let global = self.read_hort_layer(&self.config_root.join(GLOBAL_FILE))?;
        let (local, warnings) = self.read_project_layer()?;
        Ok((merge(global.unwrap_or_default(), local), warnings))
    }

    /// The project layer: `.hort.json` if the project has one, otherwise the
    /// `devcontainer.json` fallback with the advisories its mapping raises, and
    /// defaults when the project declares neither.
    fn read_project_layer(&self) -> Result<(Config, Vec<Warning>), HortError> {
        if let Some(config) = self.read_hort_layer(&self.project_dir.join(LOCAL_FILE))? {
            return Ok((config, Vec::new()));
        }

        let path = self.project_dir.join(DEVCONTAINER_DIR).join(DEVCONTAINER_FILE);
        let Some(contents) = read_layer(&path)? else {
            return Ok((Config::default(), Vec::new()));
        };
        let (mut config, warnings) =
            map_devcontainer(&contents).map_err(|error| parse_failure_in(&path, error))?;
        expand_host_paths(&mut config, &self.home);
        Ok((config, warnings))
    }

    /// One layer written in hort's own format, or `None` when the file is not
    /// there. The home shorthand is expanded here, per layer, so the merge that
    /// unions and dedupes the additive arrays compares canonical paths.
    fn read_hort_layer(&self, path: &Path) -> Result<Option<Config>, HortError> {
        let Some(contents) = read_layer(path)? else {
            return Ok(None);
        };
        let mut config = parse(&contents).map_err(|error| parse_failure_in(path, error))?;
        expand_host_paths(&mut config, &self.home);
        Ok(Some(config))
    }
}

/// Read one layer's file. A missing file is `Ok(None)`: no global config before
/// onboarding and no config at all in a project are both normal states, not
/// failures. Any other read failure names the file it could not read.
fn read_layer(path: &Path) -> Result<Option<String>, HortError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HortError::InvalidConfig {
            detail: format!("could not read {}: {error}", path.display()),
        }),
    }
}

/// Re-raise a parse failure with the file named, so the user learns which of the
/// layers is broken. Parsing always fails as `InvalidConfig`, but any other
/// variant's `Display` is an acceptable fallback.
fn parse_failure_in(path: &Path, error: HortError) -> HortError {
    let detail = match error {
        HortError::InvalidConfig { detail } => detail,
        other => other.to_string(),
    };
    HortError::InvalidConfig { detail: format!("{}: {detail}", path.display()) }
}

/// Expand the home shorthand in every host path of one layer: the rootfs, the
/// read-only mounts, and each agent's read-only auth paths.
///
/// Nothing else is touched. A cache target and the shell are paths *inside* the
/// container, where the home is the sandbox user's, not the host user's, so
/// expanding them here would quietly point them somewhere else.
fn expand_host_paths(config: &mut Config, home: &Path) {
    if let Some(rootfs) = config.rootfs.take() {
        config.rootfs = Some(expand_home(&rootfs, home));
    }
    for path in &mut config.mounts.read_only {
        *path = expand_home(path, home);
    }
    for agent in &mut config.agents {
        for path in &mut agent.auth.read_only {
            *path = expand_home(path, home);
        }
    }
}

/// Rewrite a leading `~` as the user's home: `~` alone becomes the home, `~/x`
/// becomes `<home>/x`. Another user's home (`~someone/x`) stays as written,
/// since hort never consults the user database to resolve it.
fn expand_home(value: &str, home: &Path) -> String {
    if value == "~" {
        return home.display().to_string();
    }
    match value.strip_prefix("~/") {
        Some(tail) => home.join(tail).display().to_string(),
        None => value.to_owned(),
    }
}

/// Find the project root by walking up from `start` to the first directory that
/// holds a project marker: `.hort.json`, `.devcontainer/devcontainer.json`, or
/// `.git`. The nearest marker wins, so a subproject inside a larger repository
/// resolves to the subproject. With no marker anywhere up the chain there is no
/// project, which is a different answer from "the directory you happen to be
/// standing in".
pub fn find_project_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|dir| holds_project_marker(dir)).map(Path::to_path_buf)
}

/// Whether a directory looks like a project root. Each marker is tested for mere
/// existence rather than for being a directory, because inside a git worktree
/// `.git` is a regular file pointing at the real one, and hort's own sandboxes
/// are worktrees.
fn holds_project_marker(dir: &Path) -> bool {
    dir.join(LOCAL_FILE).exists()
        || dir.join(DEVCONTAINER_DIR).join(DEVCONTAINER_FILE).exists()
        || dir.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use tempfile::TempDir;

    use crate::domain::config::CacheDir;

    /// The home the shorthand expands to. A literal path, not a scratch
    /// directory: the resolver only ever uses the home to build strings, never to
    /// read from disk, so a literal keeps every expected value literal too.
    const HOME: &str = "/home/tester";

    /// A scratch directory whose path is canonicalized, so paths built from it
    /// compare equal to the ones the discovery walk returns regardless of any
    /// symlink in the temp root.
    fn temp_dir() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    /// Write a configuration file, creating the directories above it.
    fn write_config(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn config_resolver_reads_the_global_layer() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &config_root.join("config.json"),
            r#"{ "rootfs": "/opt/hort/rootfs/devbox" }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("a readable global layer resolves");

        assert_eq!(resolved.rootfs.as_deref(), Some("/opt/hort/rootfs/devbox"));
    }

    #[test]
    fn config_resolver_prefers_the_local_layer_over_the_global() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &config_root.join("config.json"),
            r#"{ "rootfs": "/opt/hort/rootfs/global" }"#,
        );
        write_config(&project_dir.join(".hort.json"), r#"{ "rootfs": "/opt/hort/rootfs/local" }"#);
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("both layers resolve");

        assert_eq!(resolved.rootfs.as_deref(), Some("/opt/hort/rootfs/local"));
    }

    #[test]
    fn config_resolver_defaults_when_neither_layer_exists() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("no configuration at all resolves");

        assert_eq!(resolved.rootfs, None);
        assert!(resolved.mounts.read_only.is_empty());
    }

    #[test]
    fn config_resolver_falls_back_to_devcontainer_without_a_hort_file() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".devcontainer").join("devcontainer.json"),
            r#"{ "mounts": ["source=/opt/dotfiles/nvim,target=/root/.config/nvim,type=bind,readonly"] }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("a devcontainer fallback resolves");

        assert_eq!(resolved.mounts.read_only, vec!["/opt/dotfiles/nvim".to_string()]);
    }

    #[test]
    fn config_resolver_ignores_devcontainer_when_a_hort_file_exists() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "mounts": { "readOnly": ["/opt/dotfiles/from-hort"] } }"#,
        );
        write_config(
            &project_dir.join(".devcontainer").join("devcontainer.json"),
            r#"{ "mounts": ["source=/opt/dotfiles/from-devcontainer,target=/root/dotfiles,type=bind,readonly"] }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the hort layer resolves");

        assert_eq!(resolved.mounts.read_only, vec!["/opt/dotfiles/from-hort".to_string()]);
    }

    #[test]
    fn config_resolver_reports_the_devcontainer_warning() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".devcontainer").join("devcontainer.json"),
            r#"{ "image": "mcr.microsoft.com/devcontainers/base:ubuntu" }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (_resolved, warnings) = resolver.resolve().expect("a devcontainer fallback resolves");

        assert!(!warnings.is_empty());
    }

    #[test]
    fn config_resolver_expands_the_home_shorthand_in_rootfs() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "rootfs": "~/.local/share/hort/rootfs/devbox" }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(
            resolved.rootfs.as_deref(),
            Some("/home/tester/.local/share/hort/rootfs/devbox")
        );
    }

    #[test]
    fn config_resolver_expands_the_home_shorthand_in_read_only_mounts() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "mounts": { "readOnly": ["~/.config/nvim"] } }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(resolved.mounts.read_only, vec!["/home/tester/.config/nvim".to_string()]);
    }

    #[test]
    fn config_resolver_expands_the_home_shorthand_in_agent_auth_paths() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "agents": [{ "command": "claude", "auth": { "readOnly": ["~/.claude"] } }] }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(resolved.agents[0].auth.read_only, vec!["/home/tester/.claude".to_string()]);
    }

    #[test]
    fn config_resolver_expands_the_bare_home_shorthand() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(&project_dir.join(".hort.json"), r#"{ "mounts": { "readOnly": ["~"] } }"#);
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(resolved.mounts.read_only, vec!["/home/tester".to_string()]);
    }

    #[test]
    fn config_resolver_leaves_another_user_home_shorthand_untouched() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "mounts": { "readOnly": ["~someoneelse/.config/nvim"] } }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(resolved.mounts.read_only, vec!["~someoneelse/.config/nvim".to_string()]);
    }

    #[test]
    fn config_resolver_dedupes_a_mount_written_both_ways_across_layers() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &config_root.join("config.json"),
            r#"{ "mounts": { "readOnly": ["~/.config/nvim"] } }"#,
        );
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "mounts": { "readOnly": ["/home/tester/.config/nvim"] } }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("both layers resolve");

        assert_eq!(resolved.mounts.read_only, vec!["/home/tester/.config/nvim".to_string()]);
    }

    #[test]
    fn config_resolver_leaves_the_cache_target_untouched() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(
            &project_dir.join(".hort.json"),
            r#"{ "cache": { "dirs": [{ "name": "pip", "target": "~/.cache/pip" }] } }"#,
        );
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let (resolved, _warnings) = resolver.resolve().expect("the local layer resolves");

        assert_eq!(
            resolved.cache.dirs,
            vec![CacheDir::Named { name: "pip".to_string(), target: "~/.cache/pip".to_string() }]
        );
    }

    #[test]
    fn config_resolver_errs_on_malformed_local_config() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(&project_dir.join(".hort.json"), r#"{ "rootfs": "#);
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let result = resolver.resolve();

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn config_resolver_errs_on_malformed_global_config() {
        let (_config, config_root) = temp_dir();
        let (_project, project_dir) = temp_dir();
        write_config(&config_root.join("config.json"), r#"{ "rootfs": "#);
        let resolver = ConfigResolver::new(config_root, project_dir, PathBuf::from(HOME));

        let result = resolver.resolve();

        assert!(matches!(result, Err(HortError::InvalidConfig { .. })));
    }

    #[test]
    fn find_project_dir_returns_the_nearest_directory_holding_a_hort_file() {
        let (_root, root) = temp_dir();
        let project = root.join("project");
        let start = project.join("src").join("deep");
        // The repository above the project is a marker too, so it is what the
        // nearest-wins rule has to beat.
        fs::create_dir_all(root.join(".git")).unwrap();
        write_config(&project.join(".hort.json"), "{}");
        fs::create_dir_all(&start).unwrap();

        let found = find_project_dir(&start);

        assert_eq!(found, Some(project));
    }

    #[test]
    fn find_project_dir_returns_the_directory_holding_a_devcontainer_file() {
        let (_root, root) = temp_dir();
        let project = root.join("project");
        let start = project.join("src");
        write_config(&project.join(".devcontainer").join("devcontainer.json"), "{}");
        fs::create_dir_all(&start).unwrap();

        let found = find_project_dir(&start);

        assert_eq!(found, Some(project));
    }

    #[test]
    fn find_project_dir_returns_the_git_root_without_a_hort_file() {
        let (_root, root) = temp_dir();
        let repo = root.join("repo");
        let start = repo.join("src");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&start).unwrap();

        let found = find_project_dir(&start);

        assert_eq!(found, Some(repo));
    }

    #[test]
    fn find_project_dir_returns_a_worktree_whose_git_marker_is_a_file() {
        let (_root, root) = temp_dir();
        let worktree = root.join("worktree-demo");
        let start = worktree.join("src");
        // A git worktree records its gitdir in a .git file rather than a
        // directory, and every hort sandbox is a worktree.
        write_config(&worktree.join(".git"), "gitdir: /elsewhere/.git/worktrees/demo\n");
        fs::create_dir_all(&start).unwrap();

        let found = find_project_dir(&start);

        assert_eq!(found, Some(worktree));
    }

    #[test]
    fn find_project_dir_finds_no_project_without_a_marker() {
        // Assumes no project marker sits above the scratch directory, which holds
        // for a temp root under the system temp dir.
        let (_root, root) = temp_dir();
        let start = root.join("plain").join("sub");
        fs::create_dir_all(&start).unwrap();

        let found = find_project_dir(&start);

        assert_eq!(found, None);
    }
}
