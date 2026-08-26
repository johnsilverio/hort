//! The onboarding decision: what the generated global configuration says, given
//! what the host can do and what the user answered. Pure, no I/O.
//!
//! A capability the host does not have is written into the document commented
//! out, with a short note on how to enable it, so the file never quietly
//! promises something that would not apply. Rendering the document to JSONC text
//! lives here too, because that text is the only place the promise-versus-note
//! distinction is observable; the interactive prompts and the file write are the
//! effects, and they live in the command.

use crate::domain::model::{AgentChoice, Capabilities, CgroupCaps, OnboardingAnswers, Warning};

/// The prose above the opening brace, and the only place the file says what it
/// is.
const HEADER: &str = "\
// hort global configuration, written as JSONC: comments and trailing commas are
// fine here. A project's own .hort.json overrides these, key by key.
";

/// The sink this build raises a completion on.
const DESKTOP_SINK: &str = "desktop";

/// A rootfs path to show someone who does not have one yet, so the commented
/// placeholder has the shape of a real answer instead of a metavariable.
const EXAMPLE_ROOTFS: &str = "~/.local/share/hort/rootfs/devbox";

/// The global configuration file hort is about to write, as a value: every entry
/// is either active or commented out with the note that says how to enable what
/// it names.
pub struct ConfigDocument {
    entries: Vec<Entry>,
}

impl ConfigDocument {
    /// Render the document as the JSONC text of `~/.config/hort/config.json`.
    pub fn render(&self) -> String {
        let mut rendered = String::from(HEADER);
        rendered.push_str("{\n");
        for (position, entry) in self.entries.iter().enumerate() {
            if position > 0 {
                rendered.push('\n');
            }
            for line in entry.lines() {
                rendered.push_str("  ");
                rendered.push_str(&line);
                rendered.push('\n');
            }
        }
        rendered.push_str("}\n");
        rendered
    }
}

/// One setting of the generated file, with the prose that introduces it.
///
/// A setting this host cannot honour is written out commented rather than left
/// out, and the note is what tells a reader whether they are looking at
/// something already in force or at a suggestion they have to act on.
enum Entry {
    Active { note: Vec<String>, body: Vec<String> },
    Commented { note: Vec<String>, body: Vec<String> },
}

impl Entry {
    /// The entry's own text with the comment markers applied, one string per
    /// line and no indentation of its own.
    fn lines(&self) -> Vec<String> {
        match self {
            Entry::Active { note, body } => {
                note.iter().map(|line| commented(line)).chain(body.iter().cloned()).collect()
            }
            Entry::Commented { note, body } => {
                note.iter().chain(body).map(|line| commented(line)).collect()
            }
        }
    }
}

/// Decide the global configuration document for this host and these answers,
/// plus the advisories to print now.
///
/// The two outputs answer different audiences at different moments: a note
/// inside the document is what the file tells the user later, a [`Warning`] is
/// what the terminal tells them right now.
pub fn generate_config(
    caps: &Capabilities,
    answers: &OnboardingAnswers,
) -> (ConfigDocument, Vec<Warning>) {
    let document = ConfigDocument {
        entries: vec![
            rootfs_entry(answers.rootfs.as_deref()),
            mounts_entry(&answers.dotfiles),
            agents_entry(&answers.agents),
            notifications_entry(answers.notifications, caps.notify_send.is_some()),
            resources_entry(&caps.cgroup),
        ],
    };

    let mut warnings = Vec::new();
    if answers.rootfs.is_none() {
        warnings.push(Warning::new(
            "no rootfs directory was given, so 'rootfs' is written commented out: hort cannot build a sandbox until it names a prepared directory",
        ));
    }

    (document, warnings)
}

/// The one field no amount of host detection can answer.
fn rootfs_entry(rootfs: Option<&str>) -> Entry {
    match rootfs {
        Some(path) => Entry::Active {
            note: vec!["The prepared rootfs directory every sandbox is built from.".to_string()],
            body: vec![format!("\"rootfs\": {},", quoted(path))],
        },
        None => Entry::Commented {
            note: vec![
                "No prepared rootfs yet. hort runs a rootfs directory and never builds".to_string(),
                "one, so make one first, then put its path here and uncomment:".to_string(),
                "  podman export $(podman create <image>) | tar -x -C <dir>".to_string(),
                "  debootstrap stable <dir> http://deb.debian.org/debian".to_string(),
                "  umoci unpack --image <image> <bundle>, then keep <bundle>/rootfs".to_string(),
            ],
            body: vec![format!("\"rootfs\": {},", quoted(EXAMPLE_ROOTFS))],
        },
    }
}

/// The dotfiles the user picked, on their way to the read-only mount list.
fn mounts_entry(dotfiles: &[String]) -> Entry {
    Entry::Active {
        note: vec!["Host paths every sandbox mounts read-only, dotfiles and the like.".to_string()],
        body: object("mounts", array("readOnly", quoted_items(dotfiles))),
    }
}

/// The agents the user picked. A shortcut list and never a binding: the sandbox
/// still boots empty and nothing here is ever started for them.
fn agents_entry(agents: &[AgentChoice]) -> Entry {
    Entry::Active {
        note: vec![
            "Agents you typically run here. A reminder for you and not a binding:".to_string(),
            "a sandbox boots empty and you start the agent yourself.".to_string(),
        ],
        body: array("agents", agents.iter().flat_map(agent_lines).collect()),
    }
}

/// One `agents[]` entry, carrying only what the user actually has for it.
fn agent_lines(agent: &AgentChoice) -> Vec<String> {
    let mut fields = vec![format!("\"command\": {},", quoted(&agent.command))];
    if !agent.auth_read_only.is_empty() {
        let paths: Vec<String> = agent.auth_read_only.iter().map(|path| quoted(path)).collect();
        fields.push(format!(r#""auth": {{ "readOnly": [{}] }},"#, paths.join(", ")));
    }
    if agent.stop_hook {
        fields.push(r#""notify": { "stopHook": true },"#.to_string());
    }
    indented("{", fields, "},")
}

/// Where a finished agent gets announced, and whether this host can announce it.
///
/// This is the one entry the user actively says yes to and that hort would then
/// have to deliver on, so a host with nothing to raise the notification through
/// gets the entry commented out: the alternative is a promise that breaks at the
/// moment an agent finishes, which is the moment it matters.
fn notifications_entry(accepted: bool, has_notify_send: bool) -> Entry {
    let body = object("notifications", vec![format!("\"sink\": {},", quoted(DESKTOP_SINK))]);
    match (accepted, has_notify_send) {
        (true, true) => Entry::Active {
            note: vec![
                "Raised when an agent announces that it finished.".to_string(),
                "Add a \"message\" here to change the text; <name> becomes the sandbox name."
                    .to_string(),
            ],
            body,
        },
        (true, false) | (false, false) => Entry::Commented {
            note: vec![
                "notify-send was not found on this host, so a desktop notification has".to_string(),
                "nothing to go through. Install libnotify, then uncomment.".to_string(),
            ],
            body,
        },
        (false, true) => Entry::Commented {
            note: vec![
                "Uncomment to be told when an agent announces that it finished.".to_string(),
            ],
            body,
        },
    }
}

/// The per-sandbox ceiling, which stays a suggestion whatever this host
/// delegates: nobody was asked for one, and a generated file that capped a
/// sandbox would be enforcing a limit the user never chose. What delegation
/// changes is only what the note has to say.
fn resources_entry(cgroup: &CgroupCaps) -> Entry {
    let memory = ceiling_suggestion(cgroup.memory, "memory", r#""memory": "4g","#, "the memory");
    let cpus = ceiling_suggestion(cgroup.cpu, "cpu", r#""cpus": 2,"#, "the CPU");
    Entry::Active {
        note: vec!["A sandbox is capped by nothing until one of these is uncommented.".to_string()],
        body: object("resources", [memory, cpus].concat()),
    }
}

/// One commented ceiling and its note, which says how to turn the entry on where
/// the controller that would enforce it is delegated, and how to get the
/// controller where it is not.
fn ceiling_suggestion(delegated: bool, controller: &str, entry: &str, capped: &str) -> Vec<String> {
    let note = if delegated {
        vec![format!("Uncomment to cap {capped} a sandbox may use.")]
    } else {
        vec![
            format!(
                "Capping {capped} needs the {controller} controller, not delegated to this user."
            ),
            format!("Add {controller} to Delegate= in a systemd drop-in for user@.service first."),
        ]
    };
    Entry::Commented { note, body: vec![entry.to_string()] }.lines()
}

/// `"key": { ... },` with the body indented one level, collapsed to one line
/// when there is no body.
fn object(key: &str, body: Vec<String>) -> Vec<String> {
    block(key, '{', '}', body)
}

/// `"key": [ ... ],` with the body indented one level, collapsed to one line
/// when there is no body.
fn array(key: &str, body: Vec<String>) -> Vec<String> {
    block(key, '[', ']', body)
}

fn block(key: &str, open: char, close: char, body: Vec<String>) -> Vec<String> {
    if body.is_empty() {
        return vec![format!("\"{key}\": {open}{close},")];
    }
    indented(&format!("\"{key}\": {open}"), body, &format!("{close},"))
}

/// An opening line, a body one level in, and a closing line.
fn indented(open: &str, body: Vec<String>, close: &str) -> Vec<String> {
    let mut lines = vec![open.to_string()];
    lines.extend(body.into_iter().map(|line| format!("  {line}")));
    lines.push(close.to_string());
    lines
}

/// The elements of an array of strings, one per line.
fn quoted_items(values: &[String]) -> Vec<String> {
    values.iter().map(|value| format!("{},", quoted(value))).collect()
}

/// A value as a JSON string literal, so a path carrying a quote or a backslash
/// cannot break the file it is written into.
fn quoted(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

fn commented(line: &str) -> String {
    format!("// {line}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::config;
    use crate::domain::model::{AgentChoice, CgroupCaps};
    use crate::domain::resources::resource_limits;

    fn host_with_everything() -> Capabilities {
        Capabilities {
            user_ns: true,
            pasta: Some(PathBuf::from("/usr/bin/pasta")),
            ip: Some(PathBuf::from("/usr/bin/ip")),
            cgroup: CgroupCaps { memory: true, pids: true, cpu: true, cpuset: true },
            landlock_abi: Some(4),
            overlayfs_rootless: true,
            notify_send: Some(PathBuf::from("/usr/bin/notify-send")),
            git: true,
        }
    }

    fn answers_with_a_rootfs() -> OnboardingAnswers {
        OnboardingAnswers {
            rootfs: Some("~/.local/share/hort/rootfs/devbox".to_string()),
            dotfiles: Vec::new(),
            agents: Vec::new(),
            notifications: false,
        }
    }

    #[test]
    fn generate_config_only_promises_notifications_the_host_can_raise() {
        let accepted = OnboardingAnswers { notifications: true, ..answers_with_a_rootfs() };
        let host_without_notify_send = Capabilities { notify_send: None, ..host_with_everything() };

        let promised = generate_config(&host_with_everything(), &accepted).0.render();
        let withheld = generate_config(&host_without_notify_send, &accepted).0.render();

        assert_eq!(
            config::parse(&promised)
                .expect("the generated document is valid JSONC")
                .notifications
                .expect("a host that can raise a notification gets an active entry")
                .sink
                .as_deref(),
            Some("desktop")
        );
        assert!(
            withheld.contains(r#"// "notifications""#),
            "a promise hort could not keep is written commented out, never left out: {withheld}"
        );
        assert!(
            config::parse(&withheld)
                .expect("the generated document is valid JSONC")
                .notifications
                .is_none(),
            "and a commented entry is not a promise: {withheld}"
        );
    }

    #[test]
    fn a_generated_config_never_sets_a_resource_ceiling_nobody_asked_for() {
        let caps = host_with_everything();

        let (document, _warnings) = generate_config(&caps, &answers_with_a_rootfs());
        let rendered = document.render();

        assert!(rendered.contains(r#"// "memory""#), "written as a suggestion: {rendered}");
        assert!(rendered.contains(r#"// "cpus""#), "written as a suggestion: {rendered}");
        let (ceiling, _warnings) = resource_limits(
            config::parse(&rendered)
                .expect("the generated document is valid JSONC")
                .resources
                .as_ref(),
            &caps.cgroup,
        )
        .expect("the generated resources block is well formed");
        assert!(ceiling.is_none(), "a host that delegates everything is still capped by nothing");
    }

    #[test]
    fn a_commented_entry_says_how_to_enable_what_it_names() {
        let caps = Capabilities {
            cgroup: CgroupCaps { memory: true, pids: true, cpu: false, cpuset: true },
            ..host_with_everything()
        };

        let (document, _warnings) = generate_config(&caps, &answers_with_a_rootfs());
        let rendered = document.render();

        assert!(
            rendered.contains("Delegate="),
            "the note names the systemd directive that delegates the controller: {rendered}"
        );
    }

    #[test]
    fn a_commented_entry_does_not_say_how_to_enable_what_the_host_already_has() {
        let caps = host_with_everything();

        let (document, _warnings) = generate_config(&caps, &answers_with_a_rootfs());

        assert!(
            !document.render().contains("Delegate="),
            "a host that already delegates its controllers has nothing to go enable"
        );
    }

    #[test]
    fn generate_config_carries_the_dotfiles_the_user_selected() {
        let answers = OnboardingAnswers {
            rootfs: Some("~/.local/share/hort/rootfs/devbox".to_string()),
            dotfiles: vec!["~/.config/nvim".to_string(), "~/.tmux.conf".to_string()],
            agents: Vec::new(),
            notifications: false,
        };

        let (document, _warnings) = generate_config(&host_with_everything(), &answers);

        let parsed =
            config::parse(&document.render()).expect("the generated document is valid JSONC");
        assert_eq!(
            parsed.mounts.read_only,
            vec!["~/.config/nvim".to_string(), "~/.tmux.conf".to_string()]
        );
    }

    #[test]
    fn generate_config_warns_when_no_rootfs_was_given() {
        let answers = OnboardingAnswers {
            rootfs: None,
            dotfiles: Vec::new(),
            agents: Vec::new(),
            notifications: false,
        };

        let (_document, warnings) = generate_config(&host_with_everything(), &answers);

        assert!(
            warnings.iter().any(|warning| warning.to_string().contains("rootfs")),
            "the one field hort cannot infer is missing, and the terminal says so: {warnings:?}"
        );
    }

    #[test]
    fn a_missing_rootfs_is_written_as_a_commented_placeholder_naming_the_prepare_commands() {
        let answers = OnboardingAnswers {
            rootfs: None,
            dotfiles: Vec::new(),
            agents: Vec::new(),
            notifications: false,
        };

        let (document, _warnings) = generate_config(&host_with_everything(), &answers);
        let rendered = document.render();

        assert!(rendered.contains(r#"// "rootfs""#), "the field is there, commented: {rendered}");
        assert!(rendered.contains("podman export"), "and it names how to prepare one: {rendered}");
        assert!(rendered.contains("debootstrap"), "and it names how to prepare one: {rendered}");
        assert!(rendered.contains("umoci unpack"), "and it names how to prepare one: {rendered}");
    }

    #[test]
    fn the_rendered_document_parses_as_the_config_it_describes() {
        let answers = OnboardingAnswers {
            rootfs: Some("~/.local/share/hort/rootfs/devbox".to_string()),
            dotfiles: vec!["~/.config/fish".to_string()],
            agents: vec![AgentChoice {
                command: "claude --dangerously-skip-permissions".to_string(),
                auth_read_only: vec!["~/.claude".to_string()],
                stop_hook: true,
            }],
            notifications: true,
        };

        let (document, _warnings) = generate_config(&host_with_everything(), &answers);

        let parsed = config::parse(&document.render())
            .expect("hort has to be able to read the file it just wrote");
        assert_eq!(parsed.rootfs.as_deref(), Some("~/.local/share/hort/rootfs/devbox"));
        assert_eq!(parsed.agents[0].command, "claude --dangerously-skip-permissions");
        assert_eq!(parsed.agents[0].auth.read_only, vec!["~/.claude".to_string()]);
    }
}
