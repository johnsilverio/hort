<h1 align="center">hort</h1>

<p align="center">
  <strong>Give your coding agents a place to work that isn't your machine.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-AGPL_v3-00add8" alt="AGPL v3">
  <img src="https://img.shields.io/badge/language-Rust-dea584?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/platform-Linux_·_VPS-555" alt="Platform">
  <img src="https://img.shields.io/badge/status-early_development-orange" alt="Status">
</p>

> [!WARNING]
> hort is in early development. The design is settled and documented; the code is being written. The commands below describe the interface we're building toward, not a finished product. Expect things to change.

<br>

## The problem

Coding agents got good enough to leave alone. You hand one a task, it edits files, runs your tests, fixes what broke, and reports back. The natural next move is to take your hands off the keyboard and let it run.

But "let it run" means giving a non-deterministic process the same power you have. It can read your SSH keys, touch anything in your home directory, and run any command it decides to run. Most of the time that's fine. The problem is the tail: the one run where a generated command is wrong, a loop never terminates, or a repository you didn't fully trust quietly does something you didn't ask for. You only need to lose your machine once to stop wanting to roll those dice.

The usual fix is a dev container. It works, but it was built around the IDE. If you live in the terminal, the IDE-shaped sandbox asks you to give up the environment you're fast in — your shell, your editor, your keybindings — and work in someone else's instead. So most terminal people skip the sandbox and just hope the agent behaves. That's the gap hort fills: isolation that keeps your environment, whatever it is, instead of replacing it with its own.

This goes double on a server. A lot of agent work doesn't happen on a laptop at all — it happens over SSH on a Linux box or a VPS, where there's no IDE to host a dev container and you're often logged in as root. That's the worst place to hand an agent unrestricted permissions, and the place a terminal-native sandbox fits best. hort is rootless and daemonless, so it drops onto a VPS exactly the way it sits on your laptop — same workflow, same commands, no extra privilege to grant.

<br>

## The idea

The thing that makes hort different is a small distinction that turns out to matter a lot: **the sandbox and the agent are not the same object, and they don't share a lifetime.**

Almost everything in this space treats them as one. You run the tool, it spins up a container, runs one agent, and tears everything down when the agent exits. The container exists _because_ the agent exists. hort flips that. The sandbox is a place. It comes up, it stays up, and it's yours to walk in and out of. Agents are just things you run inside it, the way you run things inside any machine you own.

That inversion is the whole product, because once the box has its own life, three things you actually want stop being hard:

You can run several agents in the same project at once. One writes the feature, another reviews it, a third pokes at a failing test, all looking at the same files, because they're all just sessions inside the same box. Conflicts between them are your problem to manage, exactly as they would be if you opened three terminals on your laptop today. hort doesn't pretend to referee that, and that's the honest choice.

You can close the terminal without losing the work. The box isn't tied to the window you opened it from. Lose the SSH connection, close the tab by accident, walk away and come back tomorrow; the box is still running and you reattach to it. It's the same reason terminal multiplexers exist (tmux, zellij), applied one layer down.

You can put it on a server and finally relax. An agent running as root on a VPS is the scariest version of this whole story, and it's also where people most want the leverage. Inside hort, root in the container is still trapped in the container. The host underneath stays out of reach.

<br>

## How it works

A sandbox is two things bound together: a git worktree on your host and an embedded-libcontainer OCI container — no daemon, no Docker — that mounts it. They're born together when you bring the sandbox up, and they die together when you tear it down, and never in between. A lightweight idle process keeps the container alive so it doesn't depend on any agent running inside.

The box comes up empty. It runs nothing on its own. You drop into a shell and start whatever you want, when you want it, as many as you want. Your dotfiles are mounted read-only, so what you get is your environment — your shell (fish, zsh, bash), your editor (Neovim, Emacs, Helix, whatever you use), your keybindings — not a stripped-down stand-in. Inside the box, `cd ..` leads nowhere useful, because the host filesystem simply isn't there. Your real git repository stays on the host, untouched; what the agent sees is a disposable draft on its own branch — a worktree, not a copy of your files. If the agent ruins it, you've lost a scratch branch, not your work and not your machine.

```
┌────────────────────────────────── host ──────────────────────────────────┐
│                                                                          │
│  your git repo ──worktree──►  ┌────────────── the box ───────────────┐   │
│  (.git stays here,            │ an embedded-libcontainer OCI         │   │
│   untouched by agents)        │ container — no daemon                │   │
│                               │                                      │   │
│                               │ /workdir   the worktree (writable)   │   │
│                               │ /          read-only base layer,     │   │
│                               │            plus an ephemeral upper   │   │
│                               │            thrown away on teardown   │   │
│                               │ dotfiles   mounted read-only         │   │
│                               │ anchor     keeps the box alive       │   │
│                               │                                      │   │
│  terminal 1 ──────────────────┼─► session: your editor               │   │
│  terminal 2 ──────────────────┼─► session: an agent, writing         │   │
│  terminal 3 ──────────────────┼─► session: an agent, reviewing       │   │
│  (a pane, a tab, a window —   │                                      │   │
│   hort doesn't care which)    └──────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────┘
```

Agents that can announce when they finish a task will tell hort, and hort raises a desktop notification so you don't have to babysit it. You review the changes on the host, in your own editor with your own git, while the box is still up. You keep what's worth keeping by committing it, and then you tear the box down. Nothing about review happens inside hort, because your tools already do review better than anything hort could bolt on.

<br>

## What using it looks like

> [!NOTE]
> This is the interface we're building toward, shown so the model is clear. It isn't runnable yet.

```bash
# From a project folder: name a sandbox, bring it up, and step inside.
# This creates a branch and worktree called "api-feature".
hort up api-feature

# Your prompt tells you where you are
# (your own shell config draws this from variables hort sets):
#   you@host ~/projects/api  (api-feature) (worktree-api-feature) $

# From another terminal — a pane, a tab, or a separate window — step into the same box.
hort attach api-feature

# See what's alive.
hort ls

# Done. Tear it down. It asks first if sessions are still open.
hort down api-feature
```

Inside, you run agents the way you always have. `claude --dangerously-skip-permissions`, `aider`, `gemini`, whatever you reach for. hort doesn't wrap them or replace them. It just makes the ground underneath them safe to stand on.

<br>

## Command reference

| Command              | What it does                                                                                                                            |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `hort up <name>`     | Creates a branch and worktree named `<name>`, starts the sandbox, drops you into a shell. The box starts empty of agents. Use `--branch <existing>` to work on a branch you already have. |
| `hort up <name> -d`  | Brings the box up without stepping inside.                                                                                              |
| `hort attach <name>` | Opens a new session in a running box. Open as many as you like; where each one lives — a pane, a tab, a separate window — is your call. |
| `hort ls`            | Lists running boxes and the sessions inside them, with each box's age, idle time, and whether its worktree has uncommitted changes, so a forgotten box is easy to spot. |
| `hort down <name>`   | Tears the box down in the safe order — everything inside first (sessions, the anchor, any helpers), then the container, then the worktree. Asks for confirmation if sessions are open; `--force` skips it. |
| `hort prune`         | Cleans up idle boxes on demand, the way `docker system prune` cleans up Docker. Shows what it would remove and asks first; never runs on its own. Boxes with uncommitted changes are kept unless you force it. |

<br>

## Configuration

Configuration is layered. A global file holds your defaults; a per-project file overrides them. If a project already has a dev container, hort will read that too, so you don't have to describe the environment twice.

| Layer              | Location                          | Holds                                                                              |
| ------------------ | --------------------------------- | ---------------------------------------------------------------------------------- |
| Global             | `~/.config/hort/config.json`      | Your defaults: base rootfs, dotfiles to always mount, notification preferences.    |
| Project            | `.hort.json`                      | The environment for this project. Takes precedence.                                |
| Project (fallback) | `.devcontainer/devcontainer.json` | Read when there's no `.hort.json`, for compatibility with existing dev containers. |

A small project config reads naturally:

```jsonc
{
  // A prepared rootfs DIRECTORY with your agents baked in — a path, not an image name.
  "rootfs": "~/.local/share/hort/rootfs/devbox",
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
  // Your database: a single-destination forward to one host:port (see below).
  "network": [{ "mode": "host", "host": "127.0.0.1", "port": 5432 }],
  // Outbound access. Open by default — the agent needs it to reach its model.
  // Switch to an allowlist and the box can reach only the hosts you name
  // (plus your database); everything else has no route out.
  "egress": true, // or: { "allow": ["api.anthropic.com", "github.com"] }
  // A bare name mounts at /workdir/<name>; use the object form for caches outside the worktree.
  "cache": { "dirs": ["node_modules", { "name": "pip", "target": "~/.cache/pip" }] },
  // Optional: a per-sandbox RAM/CPU ceiling, and the shell each session opens.
  "resources": { "memory": "4g", "cpus": 2 },
  "shell": "/usr/bin/fish",
}
```

The `agents` block is a convenience list of what you tend to run here, not a rule about what the box launches. The box still starts empty; you choose.

> [!TIP]
> The `(api-feature)` part of the prompt is yours to set up. hort exports `HORT_SANDBOX` and `HORT_WORKTREE` into every session, and your shell config decides how to show them. hort hands you the information; your prompt does what it likes with it.

<br>

## Talking to your database

Real work needs a real database, and where yours runs changes how the box reaches it. hort supports both common setups, picked per project, and you can list more than one. Either way the box reaches your database the same way: a single-destination forward to one declared `host:port`, through pasta's host gateway. hort isn't a Docker client and doesn't join a Docker network — it forwards exactly the address you name, and nothing else.

| Setup     | Your Postgres runs       | hort reaches it by                                                  |
| --------- | ------------------------ | ------------------------------------------------------------------- |
| `host`    | Directly on the host     | Forwarding to the host's address (e.g. `127.0.0.1:5432`).           |
| `network` | In a container (Compose) | Forwarding to the `host:port` that container publishes on the host. |

Nothing is wired up unless you ask for it. And the rule that doesn't bend: development credentials and development databases go inside, production never does. The sandbox protects you from your agent wrecking things, not from a determined attacker stealing what you handed it.

<br>

## What it protects, and what it doesn't

It's worth being precise about the boundary, because a security tool that overstates itself is worse than none.

What the box guarantees:

|                                  | How                                                                                                      |
| -------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `cd ..` leads nowhere            | The host filesystem isn't mounted; there's nothing above the project to reach.                           |
| Your repo survives a rogue agent | The real `.git` stays on the host; the agent only ever sees a disposable worktree on its own branch.     |
| `sudo` inside buys nothing       | All Linux capabilities are dropped and the container user isn't root, so there's nothing to escalate to. |

So the box contains destruction: the filesystem and processes are walled off from the host, and the worst a rogue command can wreck is the worktree.

What it deliberately does not do:

The box stops accidents, not espionage. By default it has open internet — the agent needs it to reach its model's API — so a genuinely hostile repository running under `--dangerously-skip-permissions` can use that same connection to send whatever lives inside the box, credentials included, back out. This is the same boundary Anthropic's own dev container draws.

You can tighten it. Switch on an egress allowlist and the box can reach only the destinations you name: every other address has nowhere to go, because the namespace is given no general route out and a small proxy lets through just the hosts on your list (your declared database is the one other route). That genuinely closes the easy exfiltration paths — there's no arbitrary server left to POST to, and no raw connection to an IP that goes anywhere. But it mitigates; it is not a vault. A destination you allowed can still be misused — if `github.com` is on the list, a malicious push is still a way out — and the technique leans on reading the destination's hostname as the connection opens, which a future web change (encrypted hostnames) could take away. So the rule holds either way: run hort on code you trust. It raises the floor against accidents; it isn't a wall against a determined thief.

The box doesn't referee your own agents. Point two of them at the same files and they can step on each other, the same way two terminals in one directory can. Managing that is your call, and treating it as yours is the honest design rather than a fake guarantee.

And the world is messy, so hort expects it. Kill the box's process, delete a worktree folder by hand, kill hort mid-command, reboot the machine: none of it corrupts anything or leaves permanent junk behind. Your real repository is never in the blast radius; it lives on the host. The next time you run hort it simply sees the true state of things, shows you whatever was left over, and lets you clear it. The worst case is a leftover to tidy, never lost work and never a wedged tool.

<br>

## Platforms

| Platform     | Status               | Notes                                                                                                   |
| ------------ | -------------------- | ------------------------------------------------------------------------------------------------------- |
| Linux        | First-class          | The runtime is in-process; the isolation is built on Linux namespaces, cgroups, Landlock, and seccomp. Desktop notifications via `notify-send`. |
| macOS        | Out of scope         | The isolation is Linux-native, so macOS isn't supported. A future Linux-distinct port is possible, not promised. |
| VPS over SSH | A reason this exists | Even as root on the remote host, the agent stays inside the box.                                        |

<br>

## Requirements

Linux with unprivileged user namespaces enabled, `pasta` (from the [passt](https://passt.top/) project) on your `PATH`, a terminal, and optionally git. Without git, hort mounts the project folder directly: you lose worktree isolation but keep full container isolation. The terminal is the entire interface; there's nothing else to install or learn.

You also bring your own base rootfs — a prepared directory with the agents you use baked in. hort runs that rootfs; it doesn't install agents for you. Prepare it however you like: build a small image (a shell plus the agents you use, no hardcoded `USER` so it runs under an arbitrary uid) and `podman export` it to a directory, or use `debootstrap`/`umoci unpack`. hort runs that directory.

<br>

## What hort is not

It isn't an orchestrator — your terminal multiplexer (tmux, zellij) already does that. It isn't a TUI or a GUI, because the terminal is the point. It isn't a diff viewer, because your editor reviews code better than a bundled one would. It doesn't abstract its runtime: it embeds libcontainer in-process rather than driving Docker or anything else. It isn't armor against malicious repositories. And it isn't built around Claude Code; that's just the first agent through the door. Any CLI agent works the same way.

<br>

## License

hort is licensed under the GNU Affero General Public License v3.0. See [`LICENSE`](./LICENSE) for the full text.

In plain terms, you can use, study, change, and share it freely, including commercially. The one condition: if you modify hort and either distribute it or run it as a service over a network, you have to release your changes under the same license. That keeps hort genuinely open while making sure improvements built on top of it come back to everyone, rather than disappearing into a closed product. If you need a use the AGPL doesn't permit, a separate commercial license can be arranged.

<br>

## Contributing

Contributions are welcome once there's enough of an implementation to build on. A `CONTRIBUTING.md` with the full workflow will arrive with the first usable release. Until then, issues and design discussion are the most useful thing you can bring.

<br>

<p align="center"><sub>Built for people who live in the terminal — on a laptop or a VPS.</sub></p>
