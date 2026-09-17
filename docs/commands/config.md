# hort config

Ask what this host can do and write the global configuration.

```text
hort config [OPTIONS]

Options:
  -f, --force  Overwrite a configuration already on disk without asking
  -h, --help   Print help
```

## When it runs

- **By itself, once.** The first time you run `hort up` or `hort attach` on a terminal and there is no global configuration file yet, hort runs this dialogue first, then carries on with the command you typed. Without a terminal it does not: the command continues with no global configuration, and `hort up` then stops at `no rootfs configured ...`.
- **Whenever you run it**, to generate the file again. If `~/.config/hort/config.json` exists, it asks before overwriting, unless you pass `-f`.

`hort ls`, `down`, `prune` and `doctor` never start it.

## The dialogue

A few inline questions, answered with the keyboard. What it offers depends on what it finds on your host. A run on a host with no rootfs yet, where `~/.claude` exists:

```text
do you have a prepared rootfs directory? [y/n] no
which of these should every sandbox mount read-only?:
> [ ] ~/.config/nvim
  [ ] ~/.config/fish
  [ ] ~/.gitconfig
add Claude Code to the agents list, mounting ~/.claude read-only? [y/n] no
raise a desktop notification when an agent announces that it finished? [y/n] yes
warning: no rootfs directory was given, so 'rootfs' is written commented out: hort cannot build a sandbox until it names a prepared directory
```

1. **rootfs.** If you answer yes, it asks `path to the prepared rootfs directory` and checks it at once. A directory that could not build a sandbox is still written, with a warning carrying the same message `hort up` would print.
2. **Dotfiles.** A multi-select of `~/.config/nvim`, `~/.config/fish`, `~/.tmux.conf` and `~/.gitconfig`, showing only those that exist. Move with the arrow keys, toggle each item with Space (select as many as you like), confirm with Enter. A hint on the prompt itself is [planned](../roadmap.md#friendlier-hort-config). (Prefer individual fish files over the whole directory; see [the fish tip](../recipes.md#fish-and-a-read-only-config-directory).)
3. **Agents.** Offered only when their credentials exist on the host: Claude Code when `~/.claude` exists, written as `claude --dangerously-skip-permissions` with `~/.claude` read-only and its completion hook enabled.
4. **Notifications.** Whether to raise a desktop notification when an agent finishes.

## What it writes

A commented JSONC file. Everything your host cannot do, or that you did not ask for, is written **commented out** with a note on how to enable it, so the file never promises something that will not work. The file written by the run above:

```jsonc
// hort global configuration, written as JSONC: comments and trailing commas are
// fine here. A project's own .hort.json overrides these, key by key.
{
  // No prepared rootfs yet. hort runs a rootfs directory and never builds
  // one, so make one first, then put its path here and uncomment:
  //   podman export $(podman create <image>) | tar -x -C <dir>
  //   debootstrap stable <dir> http://deb.debian.org/debian
  //   umoci unpack --image <image> <bundle>, then keep <bundle>/rootfs
  // "rootfs": "~/.local/share/hort/rootfs/devbox",

  // Host paths every sandbox mounts read-only, dotfiles and the like.
  "mounts": {
    "readOnly": [],
  },

  // Uncomment to give each sandbox a clone of its own, so an agent can commit
  // and open a pull request from inside. The default, worktree, keeps git a
  // host activity. A token you pass in so the agent can push lives inside the
  // box with it: scope it to one repository and protect the remote's branches.
  // The clone borrows your history instead of copying it and costs about as
  // much disk as a worktree: 2.3 GB of history gave a .git of about 6 MB.
  // "git": "clone",

  // Outbound is open and unfiltered until this is uncommented. With a list, a
  // sandbox reaches only these hosts, and only through tools that read
  // HTTP_PROXY. A bare name matches exactly, and "*." covers subdomains
  // but not the name itself.
  // "egress": { "allow": ["api.example.com", "*.example.com"] },

  // Agents you typically run here. A reminder for you and not a binding:
  // a sandbox boots empty and you start the agent yourself.
  "agents": [],

  // Raised when an agent announces that it finished.
  // Add a "message" here to change the text; <name> becomes the sandbox name.
  "notifications": {
    "sink": "desktop",
  },

  // A sandbox is capped by nothing until one of these is uncommented.
  "resources": {
    // Uncomment to cap the memory a sandbox may use.
    // "memory": "4g",
    // Capping the CPU needs the cpu controller, not delegated to this user.
    // Add cpu to Delegate= in a systemd drop-in for user@.service first.
    // "cpus": 2,
  },
}
```

The commented `rootfs` is only an example path. Put your own rootfs directory there (for example the `~/.local/share/hort/devbox` from [Preparing a rootfs](../rootfs.md)), and export each rootfs into a directory of its own.

You can edit the file by hand at any time; see [Configuration](../configuration.md).

## Messages

| Message | Meaning and what to do |
| :--- | :--- |
| `hort config needs a terminal to ask you what to configure; no flag replaces it (run it from an interactive shell, or write ~/.config/hort/config.json by hand)` | You ran `hort config` without a terminal. `-f` does not help: it only means "overwrite". |
| `<path> already exists; overwrite it?` | The prompt when a configuration exists and `-f` was not given. Answering no leaves the file alone. |
| `warning: no rootfs directory was given, so 'rootfs' is written commented out: hort cannot build a sandbox until it names a prepared directory` | Prepare a rootfs and set `rootfs`. |
| `warning: rootfs ...` (any rootfs message from [`hort up`](up.md#refusals-before-anything-is-built)) | The path you gave cannot build a sandbox yet. The file names it anyway; fix the directory. |
| `configuration write failed: could not create <dir>: ...` / `configuration write failed: could not write <file>: ...` | The directory or the file could not be written. Check permissions of `~/.config`. |
