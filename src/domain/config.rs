//! Configuration data model and JSONC parsing (pure, no I/O).
//!
//! A JSONC string (JSON with comments and trailing commas) deserializes into the
//! camelCase [`Config`] tree. Locating and reading the config files is a thin
//! adapter concern, not this module's.

use serde::Deserialize;

use super::error::HortError;
use super::model::Warning;

/// The configuration of a project's sandbox environment, as parsed from one
/// JSONC layer. Field names mirror the camelCase keys of `.hort.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub rootfs: Option<String>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub mounts: Mounts,
    #[serde(default)]
    pub network: Vec<Network>,
    #[serde(default)]
    pub egress: Option<Egress>,
    #[serde(default)]
    pub notifications: Option<Notifications>,
    #[serde(default)]
    pub cache: Cache,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub resources: Option<Resources>,
}

/// Host paths mounted into the sandbox.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mounts {
    #[serde(default)]
    pub read_only: Vec<String>,
}

/// A convenience entry for an agent the user typically runs here. Never a
/// binding — the sandbox boots empty and the user triggers agents on demand.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub command: String,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub notify: Option<Notify>,
}

/// Per-agent credentials: read-only host paths to mount and host env vars to
/// forward.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    #[serde(default)]
    pub read_only: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// How an agent announces task completion so hort can raise a notification.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notify {
    #[serde(default)]
    pub stop_hook: bool,
}

/// A single-destination database forward to a `host:port` reachable from the
/// host.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    pub mode: String,
    pub host: String,
    pub port: u16,
}

/// Outbound network policy: open (`true`) or an allowlist of hostnames. The
/// hostnames stay raw strings here; turning them into validated patterns is a
/// later pure step.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Egress {
    Open(bool),
    Allowlist { allow: Vec<String> },
}

/// Top-level notification sink and message template.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notifications {
    #[serde(default)]
    pub sink: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Container for the dependency-cache directories persisted across boots.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cache {
    #[serde(default)]
    pub dirs: Vec<CacheDir>,
}

/// A persistent cache directory: a bare name (mounted under `/workdir`) or an
/// explicit `{ name, target }` for caches outside the worktree.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum CacheDir {
    Path(String),
    Named { name: String, target: String },
}

/// Per-sandbox cgroup v2 ceiling.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpus: Option<f64>,
}

/// Parse a JSONC configuration string into a [`Config`].
///
/// Comments and trailing commas are stripped before deserialization. Malformed
/// input yields [`HortError::InvalidConfig`].
pub fn parse(jsonc: &str) -> Result<Config, HortError> {
    let mut stripped = jsonc.to_owned();
    json_strip_comments::strip(&mut stripped)
        .map_err(|e| HortError::InvalidConfig { detail: e.to_string() })?;
    serde_json::from_str(&stripped).map_err(|e| HortError::InvalidConfig { detail: e.to_string() })
}

/// The final configuration after merging the global and local layers, with the
/// local layer winning. Produced by [`merge`]. `rootfs` stays optional here — it
/// is validated later by `up`, not by the merge — and the fields reuse the same
/// sub-structs [`Config`] parses into.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub rootfs: Option<String>,
    pub agents: Vec<Agent>,
    pub mounts: Mounts,
    pub network: Vec<Network>,
    pub egress: Option<Egress>,
    pub notifications: Option<Notifications>,
    pub cache: Cache,
    pub shell: Option<String>,
    pub resources: Option<Resources>,
}

/// Merge the global and local configuration layers into the final
/// [`ResolvedConfig`], with the local layer winning.
///
/// Scalars (`rootfs`, `shell`) and `egress` take the local value where it is set,
/// else the global; `egress` is replaced wholesale, so an allowlist is never
/// unioned across layers. The additive arrays union and dedupe: `mounts.readOnly`
/// and `cache.dirs` by value, `network` by `host:port`, and `agents` by
/// `command` — a colliding agent deep-merges, its `auth` lists unioning and
/// `notify` taken local-first. The `notifications` and `resources` objects
/// deep-merge field by field.
pub fn merge(global: Config, local: Config) -> ResolvedConfig {
    ResolvedConfig {
        rootfs: local.rootfs.or(global.rootfs),
        agents: merge_agents(global.agents, local.agents),
        mounts: Mounts {
            read_only: union_dedupe(global.mounts.read_only, local.mounts.read_only),
        },
        network: merge_network(global.network, local.network),
        egress: local.egress.or(global.egress),
        notifications: merge_notifications(global.notifications, local.notifications),
        cache: Cache {
            dirs: union_dedupe(global.cache.dirs, local.cache.dirs),
        },
        shell: local.shell.or(global.shell),
        resources: merge_resources(global.resources, local.resources),
    }
}

fn union_dedupe<T: PartialEq>(mut base: Vec<T>, extra: Vec<T>) -> Vec<T> {
    for item in extra {
        if !base.contains(&item) {
            base.push(item);
        }
    }
    base
}

fn merge_agents(global: Vec<Agent>, local: Vec<Agent>) -> Vec<Agent> {
    let mut merged = global;
    for incoming in local {
        match merged.iter().position(|agent| agent.command == incoming.command) {
            Some(index) => {
                let existing = merged.remove(index);
                merged.insert(index, merge_agent(existing, incoming));
            }
            None => merged.push(incoming),
        }
    }
    merged
}

fn merge_agent(global: Agent, local: Agent) -> Agent {
    Agent {
        command: local.command,
        auth: Auth {
            read_only: union_dedupe(global.auth.read_only, local.auth.read_only),
            env: union_dedupe(global.auth.env, local.auth.env),
        },
        notify: local.notify.or(global.notify),
    }
}

fn merge_network(mut global: Vec<Network>, local: Vec<Network>) -> Vec<Network> {
    for incoming in local {
        match global
            .iter()
            .position(|net| net.host == incoming.host && net.port == incoming.port)
        {
            Some(index) => global[index] = incoming,
            None => global.push(incoming),
        }
    }
    global
}

fn merge_notifications(
    global: Option<Notifications>,
    local: Option<Notifications>,
) -> Option<Notifications> {
    match (global, local) {
        (Some(global), Some(local)) => Some(Notifications {
            sink: local.sink.or(global.sink),
            message: local.message.or(global.message),
        }),
        (global, local) => local.or(global),
    }
}

fn merge_resources(global: Option<Resources>, local: Option<Resources>) -> Option<Resources> {
    match (global, local) {
        (Some(global), Some(local)) => Some(Resources {
            memory: local.memory.or(global.memory),
            cpus: local.cpus.or(global.cpus),
        }),
        (global, local) => local.or(global),
    }
}

/// Map a `devcontainer.json` (parsed as JSONC) into a hort [`Config`] plus the
/// advisories raised while doing so.
///
/// Deliberately minimal: only read-only bind mounts carry over — the host source
/// path of each becomes a `mounts.readOnly` entry. `image`, `build`, `features`,
/// and `customizations` are ignored, each raising a [`Warning`] naming the key,
/// since hort never builds images. Used only when a project has no `.hort.json`.
pub fn map_devcontainer(jsonc: &str) -> Result<(Config, Vec<Warning>), HortError> {
    let mut stripped = jsonc.to_owned();
    json_strip_comments::strip(&mut stripped)
        .map_err(|e| HortError::InvalidConfig { detail: e.to_string() })?;
    let devcontainer: DevContainer = serde_json::from_str(&stripped)
        .map_err(|e| HortError::InvalidConfig { detail: e.to_string() })?;

    let read_only = devcontainer
        .mounts
        .iter()
        .filter_map(|mount| mount.as_str().and_then(read_only_mount_source))
        .collect();

    let mut warnings = Vec::new();
    for (key, present) in [
        ("image", devcontainer.image.is_some()),
        ("build", devcontainer.build.is_some()),
        ("features", devcontainer.features.is_some()),
        ("customizations", devcontainer.customizations.is_some()),
    ] {
        if present {
            warnings.push(Warning::new(format!(
                "devcontainer.json '{key}' is ignored: hort runs a prepared rootfs and never builds images"
            )));
        }
    }

    let config = Config {
        mounts: Mounts { read_only },
        ..Config::default()
    };
    Ok((config, warnings))
}

/// The subset of `devcontainer.json` hort inspects while mapping: the `mounts`
/// list, kept as raw values so the string `--mount` form and the object form are
/// told apart at use, plus the keys hort warns about and ignores.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevContainer {
    #[serde(default)]
    mounts: Vec<serde_json::Value>,
    #[serde(default)]
    image: Option<serde_json::Value>,
    #[serde(default)]
    build: Option<serde_json::Value>,
    #[serde(default)]
    features: Option<serde_json::Value>,
    #[serde(default)]
    customizations: Option<serde_json::Value>,
}

/// Extract the host source path of a read-only bind mount written in the Docker
/// `--mount` string form, or `None` if the mount is not flagged read-only. The
/// read-only signal is the bare `readonly` token; the object mount form carries
/// no standardized read-only flag and is handled by the caller (a non-string
/// value yields no source).
fn read_only_mount_source(spec: &str) -> Option<String> {
    let mut source = None;
    let mut read_only = false;
    for token in spec.split(',') {
        match token.trim() {
            "readonly" => read_only = true,
            other => {
                if let Some(value) = other.strip_prefix("source=") {
                    source = Some(value.to_owned());
                }
            }
        }
    }
    if read_only { source } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRD_EXAMPLE: &str = r#"{
  "rootfs": "~/.local/share/hort/rootfs/devbox", // a prepared rootfs DIRECTORY, not an image
  "mounts": {
    "readOnly": ["~/.config/nvim", "~/.config/fish", "~/.tmux.conf"],
  },
  "agents": [
    {
      "command": "claude --dangerously-skip-permissions",
      "auth": { "readOnly": ["~/.claude"] },
      // For agents that support it, this lets the box notify you on completion.
      "notify": { "stopHook": true },
    },
    { "command": "aider", "auth": { "env": ["OPENAI_API_KEY"] } },
  ],
  // A DB connection is a single-destination forward via pasta's host gateway (not a Docker network).
  "network": [{ "mode": "host", "host": "127.0.0.1", "port": 5432 }],
  // Outbound access. Default (true / omitted) is OPEN: pasta bridges directly, no proxy.
  "egress": true, // or: { "allow": ["api.anthropic.com", "github.com"] }
  // Bare name -> mounted at /workdir/<name>; object form for caches outside the worktree.
  "cache": { "dirs": ["node_modules", { "name": "pip", "target": "~/.cache/pip" }] },
  // Optional: per-sandbox cgroup ceiling, and the shell each session execs.
  "resources": { "memory": "4g", "cpus": 2 },
  "shell": "/usr/bin/fish",
}"#;

    #[test]
    fn parses_prd_commented_example() {
        let config = parse(PRD_EXAMPLE).expect("the documented JSONC example must parse");

        assert_eq!(config.rootfs.as_deref(), Some("~/.local/share/hort/rootfs/devbox"));
        assert_eq!(config.mounts.read_only[0], "~/.config/nvim");
        assert_eq!(config.agents[0].command, "claude --dangerously-skip-permissions");
        assert_eq!(config.network[0].port, 5432);
        assert_eq!(config.shell.as_deref(), Some("/usr/bin/fish"));
    }

    #[test]
    fn rejects_trailing_garbage() {
        let result = parse(r#"{"rootfs": "x"} blah"#);

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), HortError::InvalidConfig { .. }));
    }

    #[test]
    fn keys_are_camel_case() {
        let config = parse(r#"{"mounts": {"readOnly": ["~/.config/nvim"]}}"#)
            .expect("camelCase keys must populate the snake_case fields");

        assert_eq!(config.mounts.read_only, vec!["~/.config/nvim".to_string()]);
    }

    #[test]
    fn merged_config_prefers_local_over_global() {
        let global = parse(r#"{ "rootfs": "~/global-rootfs" }"#).unwrap();
        let local = parse(r#"{ "rootfs": "~/local-rootfs" }"#).unwrap();

        let merged = merge(global, local);

        assert_eq!(merged.rootfs.as_deref(), Some("~/local-rootfs"));
    }

    #[test]
    fn read_only_mounts_union_across_layers() {
        let global = parse(r#"{ "mounts": { "readOnly": ["~/.config/nvim"] } }"#).unwrap();
        let local = parse(r#"{ "mounts": { "readOnly": ["~/.tmux.conf"] } }"#).unwrap();

        let merged = merge(global, local);

        assert!(merged.mounts.read_only.contains(&"~/.config/nvim".to_string()));
        assert!(merged.mounts.read_only.contains(&"~/.tmux.conf".to_string()));
    }

    #[test]
    fn agents_dedupe_by_command() {
        let global =
            parse(r#"{ "agents": [{ "command": "claude", "auth": { "env": ["GLOBAL_KEY"] } }] }"#)
                .unwrap();
        let local =
            parse(r#"{ "agents": [{ "command": "claude", "auth": { "env": ["LOCAL_KEY"] } }] }"#)
                .unwrap();

        let merged = merge(global, local);

        assert_eq!(merged.agents.len(), 1);
        assert_eq!(merged.agents[0].command, "claude");
    }

    #[test]
    fn agent_auth_deep_merges_on_command_collision() {
        let global =
            parse(r#"{ "agents": [{ "command": "claude", "auth": { "env": ["GLOBAL_KEY"] } }] }"#)
                .unwrap();
        let local =
            parse(r#"{ "agents": [{ "command": "claude", "auth": { "env": ["LOCAL_KEY"] } }] }"#)
                .unwrap();

        let merged = merge(global, local);

        assert!(merged.agents[0].auth.env.contains(&"GLOBAL_KEY".to_string()));
        assert!(merged.agents[0].auth.env.contains(&"LOCAL_KEY".to_string()));
    }

    #[test]
    fn local_egress_allowlist_replaces_global_true() {
        let global = parse(r#"{ "egress": true }"#).unwrap();
        let local = parse(r#"{ "egress": { "allow": ["api.anthropic.com"] } }"#).unwrap();

        let merged = merge(global, local);

        assert_eq!(
            merged.egress,
            Some(Egress::Allowlist { allow: vec!["api.anthropic.com".to_string()] })
        );
    }

    #[test]
    fn notifications_merge_combines_sink_and_message_across_layers() {
        let global = parse(r#"{ "notifications": { "sink": "desktop" } }"#).unwrap();
        let local =
            parse(r#"{ "notifications": { "message": "hort sandbox '<name>' finished" } }"#)
                .unwrap();

        let merged = merge(global, local);

        let notifications = merged.notifications.expect("notifications present after deep-merge");
        assert_eq!(notifications.sink.as_deref(), Some("desktop"));
        assert_eq!(notifications.message.as_deref(), Some("hort sandbox '<name>' finished"));
    }

    #[test]
    fn devcontainer_maps_readonly_mounts() {
        let devcontainer = r#"{
  "mounts": ["source=/home/me/.config/nvim,target=/root/.config/nvim,type=bind,readonly"]
}"#;

        let (config, _warnings) =
            map_devcontainer(devcontainer).expect("a valid devcontainer.json maps");

        assert!(config.mounts.read_only.contains(&"/home/me/.config/nvim".to_string()));
    }

    #[test]
    fn devcontainer_warns_and_ignores_image() {
        let devcontainer = r#"{ "image": "mcr.microsoft.com/devcontainers/base:ubuntu" }"#;

        let (config, warnings) =
            map_devcontainer(devcontainer).expect("a valid devcontainer.json maps");

        assert!(warnings.iter().any(|warning| warning.to_string().contains("image")));
        assert!(config.mounts.read_only.is_empty());
    }
}
