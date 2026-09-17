# Configuration recipes and tips

Worked setups for the things people configure first. Every key is described in full in the [Configuration](configuration.md) reference. Global settings go in `~/.config/hort/config.json`; project settings in the project's `.hort.json`.

## Dotfiles, read-only

Mount the configuration your shell and editor need. Paths under your home land at the same place under the sandbox home, `/home/hort`:

```jsonc
// ~/.config/hort/config.json
{
  "rootfs": "~/.local/share/hort/devbox",
  "mounts": {
    "readOnly": ["~/.config/nvim", "~/.gitconfig", "~/.tmux.conf", "~/.bashrc"],
  },
}
```

The programs themselves must be in the rootfs: mounting `~/.config/nvim` does nothing for a rootfs without Neovim. If you want your usual shell as the session shell, install it in the rootfs; hort uses your host `$SHELL` automatically when the rootfs has the same path, or set [`shell`](configuration.md#shell).

### Fish and a read-only config directory

Do **not** mount all of `~/.config/fish` read-only. fish writes its universal variables to `~/.config/fish/fish_variables`, and with the directory read-only every prompt prints:

```text
error: Unable to open universal variable file '/home/hort/.config/fish/fish_variables': EROFS: Read-only file system
```

Mount the parts you edit instead, and leave the directory itself writable (it is in the sandbox's in-memory home):

```jsonc
{
  "mounts": {
    "readOnly": [
      "~/.config/fish/config.fish",
      "~/.config/fish/conf.d",
      "~/.config/fish/functions",
      "~/.config/fish/completions",
    ],
  },
}
```

The same applies to any program that writes state next to its configuration: mount the files, not the directory. Note that `hort config` offers `~/.config/fish` as a whole directory; pick the individual paths by hand instead.

### Editing a mounted file while a sandbox runs

A read-only mount of a **directory** shows your edits inside running sandboxes immediately. A mount of a **single file** follows that file only while it is the same file on disk: many editors save by writing a new file and renaming it over the old one, and a running sandbox then keeps seeing the old content. New sandboxes see the new file. If you need live edits, mount the directory, or restart the sandbox.

## Agent credentials

### Claude Code

Claude Code keeps its login under `~/.claude`. Mount it read-only and declare the completion hook:

```jsonc
// ~/.config/hort/config.json
{
  "agents": [
    {
      "command": "claude --dangerously-skip-permissions",
      "auth": { "readOnly": ["~/.claude"] },
      "notify": { "stopHook": true },
    },
  ],
}
```

Also required: `IS_SANDBOX=1` in the rootfs, or Claude Code refuses its unrestricted mode because the session is uid 0 (see [Preparing a rootfs](rootfs.md#agents-that-refuse-to-run-as-root)).

Because the mount is read-only, nothing Claude Code writes under `~/.claude` from inside the sandbox is saved, and whatever it writes elsewhere in the sandbox home is gone at `hort down`. It may therefore ask again for things it normally remembers, such as whether you trust the project folder.

### Agents that take an API key

Forward the variable by name. Its value is read from your environment when the session opens and never written to disk by hort:

```jsonc
{
  "agents": [
    { "command": "aider", "auth": { "env": ["OPENAI_API_KEY"] } },
    { "command": "gemini", "auth": { "env": ["GEMINI_API_KEY"] } },
  ],
}
```

```bash
export OPENAI_API_KEY=sk-...   # in the terminal you run hort attach from
hort attach fix-login
```

If a variable is not set, the session opens without it and hort warns. Use development keys only: the agent can read anything you forward.

## Dependency caches

Keep installs across sandboxes of the same project:

```jsonc
// .hort.json
{
  "cache": {
    "dirs": [
      "node_modules",
      { "name": "npm", "target": "~/.npm" },
      { "name": "pip", "target": "~/.cache/pip" },
      { "name": "cargo-registry", "target": "~/.cargo/registry" },
    ],
  },
}
```

`node_modules` lands at `/workdir/node_modules`; the others under `/home/hort`. Every sandbox of this project shares the same directories, so two sandboxes running installs at the same moment write into the same cache. `hort prune` removes a project's caches once the project directory is gone, or with `--force`.

## A resource ceiling

```jsonc
{
  "resources": { "memory": "6g", "cpus": 4 },
}
```

Useful when agents run unattended: a runaway process hits the ceiling instead of your machine. `cpus` needs the `cpu` cgroup controller delegated to your user; `hort doctor` tells you, and [Installation](installation.md#fixing-what-doctor-reports) shows how.

## A development database

A PostgreSQL installed on the host, and a Redis published by Docker Compose on another address:

```jsonc
// .hort.json
{
  "network": [
    { "mode": "host", "host": "127.0.0.1", "port": 5432 },
    { "mode": "network", "host": "172.17.0.1", "port": 6379 },
  ],
}
```

Inside the sandbox, use `127.0.0.1:5432` and `127.0.0.1:6379`. Each database needs its own port. Development data only. See [Databases](networking.md#databases).

## An egress allowlist

Start from what your agent and your package manager need, then add hosts as the proxy log shows them refused:

```jsonc
// .hort.json
{
  "egress": {
    "allow": [
      "api.anthropic.com",
      "registry.npmjs.org",
      "github.com",
      "*.githubusercontent.com",
    ],
  },
}
```

Which hosts an agent contacts is up to that agent; check its documentation, then watch the log:

```bash
tail -f "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/hort/sandboxes/fix-login/output.log" | grep --line-buffered -E '^(allowed|refused)'
```

Rebuild the sandbox (`hort down`, `hort up`) after changing the list. Tools must honour `HTTPS_PROXY`; see [Networking and egress](networking.md#an-egress-allowlist).

## Notifications

hort cannot tell when an agent finishes by watching processes: the agent returns to its prompt and nothing exits. The agent has to announce it. Claude Code can, through a Stop hook that hort installs for you.

You need:

1. `"notify": { "stopHook": true }` on the Claude Code entry in `agents` (as in the [credentials recipe](#claude-code)),
2. `notify-send` on the host (`libnotify`) and a desktop session that shows notifications,
3. optionally, a `notifications` block to change the text:

```jsonc
{
  "notifications": {
    "sink": "desktop",
    "message": "hort: '<name>' is done, come and review",
  },
}
```

How it works: when at least one agent has `stopHook` enabled, `hort up` mounts a small channel directory at `/run/hort/notify/` and writes `/etc/claude-code/managed-settings.d/hort-notify.json` into the sandbox, a Claude Code managed-settings file whose Stop hook appends one line of JSON to `/run/hort/notify/events.jsonl`. A small process on the host, started by `hort up` and stopped by `hort down`, raises your message each time that file grows. `hort ls` also counts the last completion as activity when computing idle time.

To test it without an agent, append a line from a session:

```bash
echo '{"ts":"","event":"stop","agent":"manual"}' >> /run/hort/notify/events.jsonl
```

Things to know:

- The settings file only exists in sandboxes built after you enabled `stopHook`.
- Claude Code documents that it merges every file in that managed-settings directory; its documentation does not say whether hooks from there run alongside the hooks in your own `~/.claude` settings. If your own Stop hooks stop firing inside a sandbox, this is the reason.
- If `notify-send` is missing, or the sink is anything but `desktop`, `hort up` warns and the sandbox starts without notifications.
- A notification that fails to show (for example no desktop session over SSH) is recorded in the sandbox's `output.log`:

  ```text
  hort: a completion of this sandbox was not raised: sandbox notifications failed: /usr/sbin/notify-send raised nothing: exit status: 1
  ```

- Any other agent that can run a shell command when it finishes can append the same line. The channel is only mounted when some agent in the configuration has `stopHook` enabled.

## Showing the sandbox in your prompt

hort sets `HORT_SANDBOX` in every session and leaves drawing to your shell. With your prompt configuration mounted read-only, add a segment when the variable is set; examples for fish and bash are in [Running agents in parallel](parallel-agents.md#knowing-which-box-a-shell-is-in).

## Colors inside the sandbox

Sessions do not receive `TERM` or `COLORTERM` from your host terminal, so programs that decide on color from them (Claude Code among them) start without color. Until hort forwards them ([planned](roadmap.md#colors-inside-sessions)), set defaults inside the sandbox from a shell configuration you mount into it. For fish:

```fish
if set -q HORT_SANDBOX
    set -q TERM; or set -gx TERM xterm-256color
    set -q COLORTERM; or set -gx COLORTERM truecolor
end
```

For bash or another POSIX shell:

```bash
if [ -n "$HORT_SANDBOX" ]; then
    case "$TERM" in ""|dumb) export TERM=xterm-256color ;; esac
    export COLORTERM="${COLORTERM:-truecolor}"
fi
```

The rootfs needs the terminfo entry for the `TERM` you choose (Debian's `ncurses-base` package carries `xterm-256color`).

## Habits worth having

- **Commit before `hort up`.** The worktree starts from `HEAD`; uncommitted changes in your checkout are not in it.
- **Commit before `hort down`.** Uncommitted work in the worktree is deleted with it.
- **Name sandboxes after the task.** The name becomes the branch.
- **Run `hort ls` now and then.** A forgotten sandbox costs memory until you remove it, and `dirty` means there is work nobody committed yet.
- **Keep production out.** No production credentials, databases or tokens in any mount, variable or network entry.
