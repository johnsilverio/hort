//! Configuration data model and JSONC parsing (pure, no I/O).
//!
//! A JSONC string (JSON with comments and trailing commas) deserializes into the
//! camelCase [`Config`] tree. Locating and reading the config files is a thin
//! adapter concern, not this module's.

use serde::Deserialize;

use super::error::HortError;

/// The configuration of a project's sandbox environment, as parsed from one
/// JSONC layer. Field names mirror the camelCase keys of `.hort.json`.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
}
