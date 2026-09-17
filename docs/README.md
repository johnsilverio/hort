# hort

hort gives your coding agents a place to work that isn't your machine.

You run `hort up fix-login` inside a repository. hort cuts a new branch and a git worktree for it, starts a small container around that worktree, and drops you into a shell inside. From there you run whatever agent you like, with its permission prompts turned off, as many copies as you want. When you are done you review and commit from your own terminal on the host, and `hort down fix-login` throws the box away.

This book is the user documentation. If you are new, read it in order up to the quickstart; after that, jump to whatever you need.

## What hort is

A **sandbox** is two things that live and die together: a git worktree on your host, on its own branch, and a container that mounts that worktree at `/workdir`. hort is the container runtime itself (it embeds one), so there is no daemon, no Docker and no image pulling. You bring a prepared root filesystem with your tools and agents already in it, and every sandbox layers a disposable writable copy on top of it.

The idea everything else follows from: **the sandbox outlives every session inside it.** A sandbox is a place. It comes up with `hort up`, it stays up until you run `hort down`, and in between you walk in and out of it with `hort attach`. Closing a terminal, dropping an SSH connection or an agent exiting ends a session, never the sandbox. That is what lets you run several agents side by side in one box and come back to them later.

What you get:

- **Destruction stays in the box.** The host filesystem does not exist inside. The real `.git` stays on the host, so the worst a bad command can do is wreck one uncommitted worktree.
- **Your environment comes with you.** Dotfiles and credentials are mounted read-only, so your shell and editor config work inside.
- **Several agents at once.** Open as many sessions as you like in one sandbox, or run several sandboxes on separate branches.
- **Nothing is lost by accident.** hort never kills an idle sandbox. `hort ls` shows every box with its age, idle time and whether its worktree holds uncommitted work.
- **An optional egress allowlist.** Turn it on and a sandbox reaches only the hosts you name.

## What hort is not

hort is deliberately small. It is **not**:

- **An orchestrator.** tmux (or your terminal) arranges panes and keeps sessions around; hort only makes sandboxes and sessions. See [Running agents in parallel](parallel-agents.md).
- **A TUI or a GUI.** Everything is a command. The only interactive prompts are first-run setup and a few yes/no confirmations.
- **A diff viewer or merge tool.** You review with `git` and your editor, on the host, while the sandbox is still alive.
- **A place to run git, by default.** In the default mode git does not work inside a sandbox: the worktree points at your host repository, which the box does not have, and making that writable would let an agent rewrite history or delete branches in your real repository. The agent writes files; you commit them from the host. When you need the agent to commit and open pull requests itself, build the sandbox with `--git clone` and it gets a clone of its own, with your repository still never written from inside. See [Git inside the sandbox](git-modes.md).
- **An agent launcher or installer.** hort starts no agent and installs none. Agents are baked into your rootfs, and you start them yourself.
- **Armor against malicious repositories.** hort contains destruction. It does not promise to stop a hostile repository from sending your data out; the allowlist raises the bar, it is not a wall. See the [Security model](security.md).
- **Tied to one agent.** Claude Code, Aider, Codex CLI, Gemini CLI: whatever runs in a terminal runs in hort.
- **Cross-platform.** Linux only. A Linux VPS over SSH is a first-class case. macOS is out of scope.

The rule that follows: **run hort on code you trust, with development credentials only, never production.**

## Where to go next

- [Installation](installation.md): build the binary and check your host with `hort doctor`.
- [Preparing a rootfs](rootfs.md): the base filesystem every sandbox runs.
- [Quickstart](quickstart.md): from nothing to an agent working in a sandbox, then review, commit and tear down.
- [Working inside a hort sandbox](agents.md): a short page to hand to the agent itself.
- [Roadmap](roadmap.md): what is planned and not available yet.
