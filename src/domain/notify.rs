//! Whether a sandbox gets a completion channel, and what it takes to make one
//! work inside the box. A pure decision over the configuration: hort never
//! watches an agent, so an agent that finished has to say so itself, and what
//! each agent can say is what the configuration declares.

use std::path::{Path, PathBuf};

use crate::domain::config::ResolvedConfig;
use crate::domain::error::HortError;
use crate::domain::model::{SandboxName, Warning};
use crate::ports::{MountAccess, Notifier, NotifyWatcher, SandboxFile, SandboxMount};

/// Where the box finds the channel a completion hook writes into. Fixed rather
/// than configurable: the hook command hort installs names this path, and nothing
/// inside the box gets to choose it.
pub const CHANNEL_DIR: &str = "/run/hort/notify";

/// The file a completion is appended to, one JSON object per line, so that
/// noticing a completion is noticing an append and never a parse.
pub const EVENTS_FILE: &str = "events.jsonl";

/// The sink the build ships. It is also the one a channel is raised on when the
/// configuration names none, so a configuration naming anything else has asked
/// for something this build cannot do.
const DESKTOP_SINK: &str = "desktop";

/// The message a completion is raised with when the configuration names none.
/// A default and not a promise: the configuration replaces it and nothing in the
/// product quotes it back.
const DEFAULT_MESSAGE: &str = "hort sandbox '<name>' finished";

/// What the message template has filled in: the sandbox the completion belongs
/// to.
const NAME_PLACEHOLDER: &str = "<name>";

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
            .unwrap_or_else(|| DESKTOP_SINK.to_string())
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

/// The message a completion of `name` is raised with: the configured template
/// with the sandbox it belongs to filled in, or the one the build ships when the
/// configuration names none.
///
/// Rendered here and once, because the sink is handed a finished message and the
/// process that would otherwise do the filling in is the one nothing can watch.
pub fn render_notification(template: Option<&str>, name: &SandboxName) -> String {
    template.unwrap_or(DEFAULT_MESSAGE).replace(NAME_PLACEHOLDER, name.as_str())
}

/// Why nothing will be raised for a declared channel on this host, and `None`
/// when something will.
///
/// A sink the build has no implementation for, or a desktop with no program to
/// raise a notification through, both leave the sandbox standing and the channel
/// mute: a missing notification is worth a line on the way out, never a box torn
/// down.
pub fn notify_degradation_warning(sink: &str, notify_send: Option<&Path>) -> Option<Warning> {
    if sink != DESKTOP_SINK {
        return Some(Warning::new(format!(
            "this build raises a completion on the desktop and nowhere else, so nothing will be raised on the '{sink}' this configuration asks for"
        )));
    }
    notify_send.is_none().then(|| {
        Warning::new(
            "notify-send is not on PATH, so no completion of this sandbox will be raised on the desktop (install libnotify to get it)",
        )
    })
}

/// Raise `message` once for every completion appended to the channel, until the
/// channel is gone.
///
/// The whole of what the watcher process does, and it terminates, because the
/// channel goes away with the sandbox. A notification that fails does not end it:
/// it is a long-lived process, so a failure is left in the sandbox's log and the
/// next completion is still raised.
pub fn watch_and_notify(
    watcher: &mut dyn NotifyWatcher,
    notifier: &dyn Notifier,
    message: &str,
) -> Result<(), HortError> {
    while watcher.wait_for_append()? {
        if let Err(refused) = notifier.notify(message) {
            // Written to this process's own streams, which are the sandbox's log:
            // the box has hours of work left in it, and a desktop that was not
            // there for one completion is no reason to stop hearing the rest.
            eprintln!("hort: a completion of this sandbox was not raised: {refused}");
        }
    }
    Ok(())
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
    use crate::fakes::{RecordingNotifier, ScriptedNotifyWatcher};
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
    fn a_template_naming_the_sandbox_is_rendered_with_the_name_it_belongs_to() {
        let rendered =
            render_notification(Some("<name> is done"), &SandboxName::new("demo").unwrap());

        // The one substitution the configuration is promised. Told about a box by
        // no name, a person with four of them open learns nothing from being
        // told one of them finished.
        assert_eq!(rendered, "demo is done");
    }

    #[test]
    fn a_channel_with_no_template_is_raised_with_the_message_the_build_ships() {
        let rendered = render_notification(None, &SandboxName::new("demo").unwrap());

        // Asserted as a literal, and it is a default rather than a promise: the
        // configuration replaces it, nothing in the product quotes it back, and
        // no error catalog carries it.
        assert_eq!(rendered, "hort sandbox 'demo' finished");
    }

    #[test]
    fn a_sink_this_build_does_not_implement_warns_that_nothing_will_be_raised() {
        let warning =
            notify_degradation_warning("webhook", Some(Path::new("/usr/bin/notify-send")));

        // The build ships one sink. A configuration naming another has asked for
        // something that will never happen, and silence here is a person waiting
        // on a notification that was never going to come.
        assert!(warning.is_some());
    }

    #[test]
    fn a_host_without_the_program_a_desktop_notification_needs_warns_about_it() {
        let warning = notify_degradation_warning("desktop", None);

        assert!(warning.is_some());
    }

    #[test]
    fn a_host_that_can_raise_the_configured_sink_warns_about_nothing() {
        let warning =
            notify_degradation_warning("desktop", Some(Path::new("/usr/bin/notify-send")));

        // An advisory on every build of every host that has what it needs is one
        // the user learns to scroll past, taking the next one that matters with
        // it.
        assert!(warning.is_none());
    }

    #[test]
    fn an_append_on_the_channel_raises_the_message_once() {
        let mut watcher = ScriptedNotifyWatcher::appending(1);
        let notifier = RecordingNotifier::new();

        watch_and_notify(&mut watcher, &notifier, "hort sandbox 'demo' finished").unwrap();

        assert_eq!(notifier.messages(), vec!["hort sandbox 'demo' finished".to_string()]);
    }

    #[test]
    fn two_appends_on_the_channel_raise_the_message_twice() {
        let mut watcher = ScriptedNotifyWatcher::appending(2);
        let notifier = RecordingNotifier::new();

        watch_and_notify(&mut watcher, &notifier, "hort sandbox 'demo' finished").unwrap();

        // One agent finishing twice is two things the user is waiting on, and
        // there is no window in which the second is the first said again: hort
        // holds no clock here and no spec sets one.
        assert_eq!(notifier.messages().len(), 2);
    }

    #[test]
    fn a_channel_that_is_gone_ends_the_watch() {
        let mut watcher = ScriptedNotifyWatcher::gone();
        let notifier = RecordingNotifier::new();

        let result = watch_and_notify(&mut watcher, &notifier, "hort sandbox 'demo' finished");

        // The channel goes away with the sandbox, which is what `down` does to it
        // while the watcher is still holding it. Leaving is the answer; a watcher
        // that span here would outlive every box on the machine.
        assert_eq!(result, Ok(()));
        assert!(notifier.messages().is_empty());
    }

    #[test]
    fn a_notification_that_fails_leaves_the_watch_running() {
        let mut watcher = ScriptedNotifyWatcher::appending(2);
        let notifier = RecordingNotifier::failing();

        let result = watch_and_notify(&mut watcher, &notifier, "hort sandbox 'demo' finished");

        // A desktop that was not there for one completion is not a reason to stop
        // hearing about the rest: this process lives as long as the sandbox does,
        // and the sandbox has hours of work left in it.
        assert_eq!(result, Ok(()));
        assert_eq!(notifier.messages().len(), 2);
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
