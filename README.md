<p align="center">
  <img width="640" height="320" alt="hort cli tool logo" src="https://github.com/user-attachments/assets/95763b65-1875-443c-8e4e-5f9d6d885f4a" />
</p>
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
> hort is in early development. The design is settled; the code is being written. What follows is the tool we're building toward, not a finished product.

<br>

## The problem

Coding agents are finally good enough to leave alone. You hand one a task, it edits files, runs the tests, fixes what broke, and reports back. The obvious next step is to take your hands off the keyboard and let it work.

The catch is that "let it work" means giving a non-deterministic process the same reach you have: your SSH keys, your whole home directory, any command it decides to run. Most of the time that's fine. The problem is the one run where a command is wrong, a loop never ends, or a repo you didn't fully trust does something you never asked for. You only have to lose your machine once to stop taking that bet.

The usual fix is a dev container, which works but is built around an IDE. A lot of people don't work that way. They live in a terminal, often over SSH on a Linux server where there's no IDE at all and the agent runs as root. That's where the risk is highest and where an IDE-shaped sandbox fits worst.

hort fills that gap: real isolation that keeps the environment you're fast in, driven entirely from the command line.

<br>

## The idea

Almost everything in this space ties the sandbox to the agent. You run the tool, it starts a container, runs one agent, and tears the whole thing down when the agent exits. hort splits them apart. The sandbox is a place. It comes up, it stays up, and you walk in and out of it. Agents are just things you run inside, the same way you run anything on a machine you own.

That single change is what makes the rest possible:

- **Run agents in parallel.** Point several at the same project at once: one writing a feature, one reviewing it, one chasing a failing test, all looking at the same files.
- **Detach and come back.** The box isn't tied to the terminal you opened it from. Drop the SSH connection or close the tab and it keeps running; reattach whenever you want.
- **Be root on a server without the fear.** Run it on a VPS and the agent can have root inside the box while your host stays completely out of reach.

<br>

## What makes it good to use

**It runs your setup, not a generic one.** Your dotfiles are mounted read-only, so your shell, your editor, and your keybindings come with you. The box feels like your machine because it is running your machine's config.

**It's light.** There's no background daemon the way Docker has one. The read-only base image is shared by every box, so running ten of them at once doesn't copy it ten times. Each box is held open by a single idle process and nothing more, so the overhead hort itself adds is small.

**It's quick to start and trivial to install.** hort is one static binary, written in Rust, that you drop into `~/.local/bin`. There's no daemon to reach and no image to pull, because you bring a prepared root filesystem, so a box comes up fast.

**Your repository is never on the line.** The real `.git` stays on the host. The agent only ever sees a throwaway worktree on its own branch, so the worst a bad command can do is ruin a scratch branch. Capabilities are dropped and the container user isn't root, so `sudo` inside gets you nothing.

<br>

## What it protects, and what it doesn't

A security tool that oversells itself is worse than none, so here is the line drawn plainly.

hort contains destruction. The filesystem and the processes are walled off from the host, and the most a rogue command can wreck is the worktree, never the machine and never your real repository.

It does not promise to stop exfiltration. By default the box has open internet, because the agent needs it to reach its model, and a genuinely hostile repo could use that same connection to send data back out. You can turn on an egress allowlist, and the box will then reach only the hosts you name, which closes the easy paths. It raises the floor; it is not a vault, and a host you allowed can still be misused.

So the rule is simple: run hort on code you trust, with development credentials only and never production. It protects you from your agent's mistakes, not from a determined attacker stealing what you put in front of it.

<br>

## A look at it

```bash
hort up api-feature      # branch + worktree, start the box, drop you in
hort attach api-feature  # open another session from any terminal
hort ls                  # see what's running
hort down api-feature    # tear it down (asks first if sessions are open)
```

Inside, you run agents however you already do: `claude`, `aider`, `gemini`, whatever you reach for. hort doesn't wrap them or replace them. It just makes the ground under them safe to stand on.

<br>

## Platforms and requirements

Linux only; the isolation is built directly on Linux kernel features, and running on a VPS over SSH is a first-class case rather than an afterthought. macOS is out of scope.

You need unprivileged user namespaces enabled, [`pasta`](https://passt.top/) on your `PATH`, and optionally git (without it you lose per-branch worktrees but keep full container isolation). You also bring a base root filesystem with your agents baked in; hort runs it, it doesn't build it for you.

<br>

## License

hort is licensed under the GNU Affero General Public License v3.0. You can use, study, change, and share it freely, including commercially. The one condition: if you modify hort and either distribute it or run it as a network service, you release your changes under the same license. Full text in [`LICENSE`](./LICENSE).

<br>

<p align="center"><sub>Built for people who work in the terminal, on a laptop or a server.</sub></p>
