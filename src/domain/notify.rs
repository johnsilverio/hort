//! Whether a sandbox gets a completion channel, and what it takes to make one
//! work inside the box. A pure decision over the configuration: hort never
//! watches an agent, so an agent that finished has to say so itself, and what
//! each agent can say is what the configuration declares.

use std::path::{Path, PathBuf};

use crate::domain::config::ResolvedConfig;
use crate::ports::{MountAccess, SandboxFile, SandboxMount};

/// Where the box finds the channel a completion hook writes into. Fixed rather
/// than configurable: the hook command hort installs names this path, and nothing
/// inside the box gets to choose it.
pub const CHANNEL_DIR: &str = "/run/hort/notify";

/// The file a completion is appended to, one JSON object per line, so that
/// noticing a completion is noticing an append and never a parse.
pub const EVENTS_FILE: &str = "events.jsonl";

/// The sink the build ships, raised for a channel whose configuration names none.
const DEFAULT_SINK: &str = "desktop";

/// The one settings file the announcing agent reads inside the box. The vendor
/// fixes this path, which is what makes several declarers collide on one file.
const CLAUDE_SETTINGS_PATH: &str = "/etc/claude-code/managed-settings.d/hort-notify.json";

/// What the template below has filled in: the announcing agent, and the file its
/// completions are appended to.
const AGENT_PLACEHOLDER: &str = "<agent>";
const EVENTS_PLACEHOLDER: &str = "<events>";

/// The settings file that turns a finished agent into an append on the channel,
/// written out as the bytes that land in the box rather than assembled from a
/// serializer: another product parses this file, so neither the order of its keys
/// nor the escaping inside them is hort's to choose.
///
/// The command is a shell one-liner and nothing else, because a shell is all a
/// prepared rootfs has to provide. Without `date` the timestamp comes out empty,
/// which is still valid JSON and still an append, and nothing ever reads the
/// line's contents.
const CLAUDE_STOP_HOOK_TEMPLATE: &str = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"ts=$(date -u +%FT%TZ 2>/dev/null); echo '{\"ts\":\"'$ts'\",\"event\":\"stop\",\"agent\":\"<agent>\"}' >> <events>"}]}]}}"#;

/// Whether any configured agent can announce that it finished.
///
/// A `notify` object that enables nothing is not a channel. The channel is a
/// writable bind, so handing one to an unrestricted agent buys a completion
/// nobody emits and costs a writable surface.
pub fn channel_is_declared(config: &ResolvedConfig) -> bool {
    announcing_agents(config).next().is_some()
}

/// The sink a declared channel is raised on, and `None` when nothing announces a
/// completion for it to raise.
pub fn channel_sink(config: &ResolvedConfig) -> Option<String> {
    channel_is_declared(config).then(|| {
        config
            .notifications
            .as_ref()
            .and_then(|notifications| notifications.sink.clone())
            .unwrap_or_else(|| DEFAULT_SINK.to_string())
    })
}

/// The writable bind that puts the channel kept at `host_dir` inside the box.
pub fn channel_mount(host_dir: &Path) -> SandboxMount {
    SandboxMount {
        source: host_dir.to_path_buf(),
        target: PathBuf::from(CHANNEL_DIR),
        access: MountAccess::ReadWrite,
    }
}

/// The settings files that make a configured agent announce its completions into
/// the channel, and nothing when no agent announces anything.
///
/// The vendor fixes the path, so several agents declaring the same hook still
/// produce one file.
pub fn stop_hook_drop_ins(config: &ResolvedConfig) -> Vec<SandboxFile> {
    announcing_agents(config)
        .next()
        .map(|agent| SandboxFile {
            path: PathBuf::from(CLAUDE_SETTINGS_PATH),
            content: stop_hook_settings(agent),
        })
        .into_iter()
        .collect()
}

/// The command of every agent that announces its own completions, in the order
/// the configuration lists them. One question asked in one place: what the mount,
/// the drop-in and the recorded sink all turn on.
fn announcing_agents(config: &ResolvedConfig) -> impl Iterator<Item = &str> {
    config
        .agents
        .iter()
        .filter(|agent| agent.notify.as_ref().is_some_and(|notify| notify.stop_hook))
        .map(|agent| agent.command.as_str())
}

/// The settings file that makes `agent` announce its completions, filled in.
fn stop_hook_settings(agent: &str) -> String {
    let events = Path::new(CHANNEL_DIR).join(EVENTS_FILE);
    // The channel goes in first, so that an agent whose command happens to spell
    // the other placeholder stays a command instead of becoming a second address.
    CLAUDE_STOP_HOOK_TEMPLATE
        .replace(EVENTS_PLACEHOLDER, &events.display().to_string())
        .replace(AGENT_PLACEHOLDER, agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::domain::config::{Agent, Auth, Cache, Mounts, Notify};
    use crate::ports::MountAccess;

    /// The settings file the vendor's schema asks for, with the one-liner that
    /// appends a completion to the channel. Written out by hand rather than
    /// assembled, because another product parses it: what is asserted has to be
    /// the bytes that land in the box, down to the escaping.
    ///
    /// The one-liner uses a shell and nothing else, since a shell is all the
    /// rootfs contract promises. A rootfs without `date` therefore leaves the
    /// timestamp empty, which is still valid JSON and still an append, and
    /// nothing in hort ever reads the line's contents.
    const CLAUDE_STOP_HOOK: &str = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"ts=$(date -u +%FT%TZ 2>/dev/null); echo '{\"ts\":\"'$ts'\",\"event\":\"stop\",\"agent\":\"claude\"}' >> /run/hort/notify/events.jsonl"}]}]}}"#;

    /// One configured agent, with its completion hook enabled or not.
    fn agent(command: &str, stop_hook: Option<bool>) -> Agent {
        Agent {
            command: command.to_string(),
            auth: Auth::default(),
            notify: stop_hook.map(|stop_hook| Notify { stop_hook }),
        }
    }

    /// A configuration whose only content is the agents the user typically runs
    /// here.
    fn config_running(agents: Vec<Agent>) -> ResolvedConfig {
        ResolvedConfig {
            rootfs: Some("/base/rootfs".to_string()),
            agents,
            mounts: Mounts::default(),
            network: Vec::new(),
            egress: None,
            notifications: None,
            cache: Cache::default(),
            shell: None,
            resources: None,
        }
    }

    #[test]
    fn an_agent_with_its_stop_hook_enabled_declares_a_channel() {
        let config = config_running(vec![agent("claude", Some(true))]);

        assert!(channel_is_declared(&config));
    }

    #[test]
    fn an_agent_whose_notify_enables_nothing_declares_no_channel() {
        let config = config_running(vec![agent("claude", Some(false))]);

        // The mount this gates is writable, and an agent running without
        // restrictions inside the box is exactly who would find it. A channel
        // nothing ever writes to is a writable surface bought for nothing.
        assert!(!channel_is_declared(&config));
    }

    #[test]
    fn the_channel_is_carried_in_writable_where_the_hook_writes() {
        let mount = channel_mount(Path::new("/state/sandboxes/demo/notify"));

        // The hook runs inside the box and writes; a read-only bind would make
        // every completion fail silently. The target is fixed because the
        // command hort installs names it, and it is the one mount whose host
        // side is a directory hort makes rather than one the user declared.
        assert_eq!(
            mount,
            SandboxMount {
                source: PathBuf::from("/state/sandboxes/demo/notify"),
                target: PathBuf::from("/run/hort/notify"),
                access: MountAccess::ReadWrite,
            }
        );
    }

    #[test]
    fn a_declared_stop_hook_drops_the_settings_file_the_agent_reads() {
        let config = config_running(vec![agent("claude", Some(true))]);

        let dropped = stop_hook_drop_ins(&config);

        // Another product parses this file, so its shape is that product's and
        // not hort's to arrange: the path, the event name and the nesting are
        // read from the vendor's documentation, and the whole point of asserting
        // the literal is that a file the agent cannot read fails in complete
        // silence, inside a box, with nothing on the host to look at.
        assert_eq!(
            dropped,
            vec![SandboxFile {
                path: PathBuf::from("/etc/claude-code/managed-settings.d/hort-notify.json"),
                content: CLAUDE_STOP_HOOK.to_string(),
            }]
        );
    }

    #[test]
    fn an_agent_whose_notify_enables_nothing_drops_no_settings_file() {
        let config = config_running(vec![agent("claude", Some(false))]);

        // The file would install a hook writing to a channel that is not
        // mounted, so every completion inside the box would fail on a path that
        // is not there.
        assert!(stop_hook_drop_ins(&config).is_empty());
    }

    #[test]
    fn the_dropped_hook_announces_the_agent_the_configuration_names() {
        let config =
            config_running(vec![agent("claude --dangerously-skip-permissions", Some(true))]);

        let dropped = stop_hook_drop_ins(&config);

        // The command is how the configuration identifies an agent, and it is
        // rarely the bare name: running one without permission limits is the
        // whole reason hort exists, so the flag is part of what the user wrote.
        // A completion announcing a name hort chose instead of the one the user
        // declared is unreadable the moment two of them write here.
        assert_eq!(
            dropped,
            vec![SandboxFile {
                path: PathBuf::from("/etc/claude-code/managed-settings.d/hort-notify.json"),
                content: r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"ts=$(date -u +%FT%TZ 2>/dev/null); echo '{\"ts\":\"'$ts'\",\"event\":\"stop\",\"agent\":\"claude --dangerously-skip-permissions\"}' >> /run/hort/notify/events.jsonl"}]}]}}"#
                    .to_string(),
            }]
        );
    }

    #[test]
    fn two_agents_declaring_a_stop_hook_drop_one_file_naming_the_first() {
        let config = config_running(vec![agent("claude", Some(true)), agent("codex", Some(true))]);

        let dropped = stop_hook_drop_ins(&config);

        // The vendor fixes the path, so two declarers collide by construction
        // and one of them has to win. The first in the configuration wins, which
        // is an answer that is the same on every run; there is no arrangement in
        // which two files at one path is an answer at all.
        assert_eq!(
            dropped,
            vec![SandboxFile {
                path: PathBuf::from("/etc/claude-code/managed-settings.d/hort-notify.json"),
                content: CLAUDE_STOP_HOOK.to_string(),
            }]
        );
    }
}
