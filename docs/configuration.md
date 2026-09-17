# Configuration

hort reads its configuration from up to two files, merges them, and uses the result every time it builds a sandbox or opens a session. This page is the reference for every key. For worked examples of common setups, see [Configuration recipes and tips](recipes.md).

## Files and layers

| Layer | Location | Typical contents |
| :--- | :--- | :--- |
| Global | `~/.config/hort/config.json` (`$XDG_CONFIG_HOME/hort/config.json` when that variable is set) | Your default `rootfs`, dotfiles, agents and their credentials, notifications. |
| Project | `.hort.json` in the project directory | What this project needs: caches, databases, an egress allowlist, a different rootfs. |
| Project, fallback | `.devcontainer/devcontainer.json` in the project directory | Read **only** when the project has no `.hort.json`. |

The **project directory** is the nearest directory, starting where you run hort and walking up, that holds `.hort.json`, `.devcontainer/devcontainer.json` or `.git`. The project file is read from your host checkout, so it does not need to be committed.

Missing files are fine: no global file and no project file both mean defaults (and no rootfs, which `hort up` refuses). A file that exists but cannot be parsed stops `hort up` and `hort attach` with a message naming it:

```text
invalid config: /home/you/src/webapp/.hort.json: expected value at line 1 column 13
```

`hort ls`, `hort down` and `hort prune` do not read the configuration, so a broken file never stops you from listing or tearing down sandboxes. `hort doctor` reports it as a row.

When to change what: configuration is read on every `up` and every `attach`. A sandbox that is already running keeps the mounts, caches, resources and network it was built with; `hort down` and `hort up` it to apply changes to those. `shell` and `agents[].auth.env` apply to the next session you open.

### Format

Both files are **JSONC**: JSON that also allows `//` and `/* */` comments and trailing commas. All keys are **camelCase**.

Keys hort does not know are **ignored silently**, so a typo such as `"readonly"` for `"readOnly"` does nothing. If something you configured has no effect, check the spelling first.

A leading `~/` is expanded to your host home in `rootfs`, `mounts.readOnly` and `agents[].auth.readOnly`. Environment variables such as `$HOME` are not expanded anywhere.

## How layers merge

The project layer wins over the global one:

| Key | Rule |
| :--- | :--- |
| `rootfs`, `shell`, `git` | The project value replaces the global one. |
| `egress` | The project value replaces the global one **entirely**. Allowlists are never combined, so a global `true` can never loosen a project allowlist. |
| `mounts.readOnly`, `cache.dirs` | Both lists are combined, duplicates removed. |
| `agents` | Both lists are combined. Entries with the same `command` are merged: their `auth.readOnly` and `auth.env` lists are combined, and the project's `notify` wins. |
| `network` | Both lists are combined. An entry with the same `host` and `port` as a global one replaces it. |
| `notifications`, `resources` | Merged field by field; the project wins where both set a field. |

A project file cannot remove an entry the global file adds to a list.

## Complete example

A global file:

```jsonc
// ~/.config/hort/config.json
{
  // The prepared rootfs every sandbox runs, unless a project says otherwise.
  "rootfs": "~/.local/share/hort/devbox",

  // Dotfiles every sandbox gets, read-only.
  "mounts": {
    "readOnly": ["~/.config/nvim", "~/.gitconfig", "~/.tmux.conf"],
  },

  "agents": [
    {
      "command": "claude --dangerously-skip-permissions",
      "auth": { "readOnly": ["~/.claude"] },
      "notify": { "stopHook": true },
    },
    { "command": "aider", "auth": { "env": ["OPENAI_API_KEY"] } },
  ],

  "notifications": {
    "sink": "desktop",
    "message": "hort sandbox '<name>' finished",
  },
}
```

A project file:

```jsonc
// ~/src/webapp/.hort.json
{
  // Dependency caches that survive down and up.
  "cache": {
    "dirs": ["node_modules", { "name": "pip", "target": "~/.cache/pip" }],
  },

  // The development database, reached at 127.0.0.1:5432 inside the sandbox.
  "network": [{ "mode": "host", "host": "127.0.0.1", "port": 5432 }],

  // Only these hosts, through the proxy.
  "egress": {
    "allow": ["api.anthropic.com", "registry.npmjs.org", "github.com", "*.githubusercontent.com"],
  },

  // A ceiling per sandbox.
  "resources": { "memory": "4g", "cpus": 2 },

  // Give each sandbox its own clone, so an agent can commit and open a pull
  // request from inside. The default, "worktree", keeps git a host activity.
  "git": "clone",

  // The login shell of every session; must exist in the rootfs.
  "shell": "/bin/bash",
}
```

## Key reference

### `rootfs`

**string.** Default: none. Merge: replaces.

The prepared rootfs directory every sandbox of this project runs. Use an absolute path or `~/...`. Required for `hort up`. See [Preparing a rootfs](rootfs.md).

### `shell`

**string.** Default: your host `$SHELL` if that same path exists inside the rootfs, otherwise `/bin/sh`. Merge: replaces.

The shell each session runs, as a login shell (`-l`) in `/workdir`. It is a path **inside** the rootfs, not expanded. `hort up` refuses a `shell` the rootfs does not have:

```text
shell '/bin/bash' not found in rootfs '/home/you/.local/share/hort/devbox' — set "shell" to one the rootfs provides, or omit it
```

### `git`

**string.** Default: `"worktree"`. Merge: replaces.

How a sandbox of this project gets git into `/workdir`.

| Value | Meaning |
| :--- | :--- |
| `"worktree"` | `/workdir` is a git worktree of your repository. git does not work inside the sandbox; you commit from the host. |
| `"clone"` | `/workdir` is a clone of your repository, with a writable `.git` of its own, so an agent can commit, branch and open a pull request from inside. |

**Before you turn `"clone"` on for a project, two things are worth knowing.** Any credential you pass into the sandbox so the agent can push lives *inside* the box, with the agent, so scope it to one repository and protect the branches on the remote ([security](security.md#in-clone-mode)). And the clone borrows your history rather than copying it, so it costs about as much disk as a worktree: measured on a repository with 2.3 GB of history, the sandbox's own `.git` was about 6 MB and the new disk was the checkout alone. See [git inside the sandbox](git-modes.md) for the whole picture, including what `hort down` does not clean up yet.

`hort up --git <mode>` overrides this for one build. In a project that is not a git repository the key has nothing to act on, so a configured `"clone"` warns and the sandbox is built with the project folder mounted:

```text
warning: the configured 'clone' git mode needs a git repository and this project is not one, so the sandbox mounts the project folder itself
```

Clone mode puts a token inside the box if you want the agent to push, so read [Git inside the sandbox](git-modes.md) before turning it on for a project.

### `mounts`

**object.** Default: `{}`.

#### `mounts.readOnly`

**array of strings.** Default: `[]`. Merge: combined.

Host paths (files or directories) mounted read-only into every sandbox, usually dotfiles. A path under your host home is placed at the same relative path under the sandbox home: `~/.config/nvim` appears at `/home/hort/.config/nvim`. Any other path keeps its absolute path. A path that does not exist on the host is skipped with a warning:

```text
warning: read-only mount '/home/you/.config/nvim' is not on this host, so the sandbox starts without it
```

Mounting a whole directory read-only means programs cannot write inside it. Some shells and editors write state into their config directory; see [the fish tip](recipes.md#fish-and-a-read-only-config-directory).

### `agents`

**array of objects.** Default: `[]`. Merge: combined, entries with the same `command` merged.

The agents you run in this project and what they need. This list is **not** a launcher: a sandbox starts empty and you run the agent yourself. What an entry does is carry that agent's credentials and completion hook into every sandbox.

#### `agents[].command`

**string, required.** The command you run, for example `claude --dangerously-skip-permissions`. It identifies the entry for merging and is recorded in completion events. hort never runs it.

#### `agents[].auth.readOnly`

**array of strings.** Default: `[]`. Host paths holding the agent's credentials, mounted read-only exactly like `mounts.readOnly` (same placement under `/home/hort`, same warning when absent). A missing credential never stops a build; the agent will ask you to log in.

#### `agents[].auth.env`

**array of strings.** Default: `[]`. Names of host environment variables to copy into every session, for API keys. The value is read from the environment `hort attach` (or `hort up`) runs in, at the moment the session opens. A variable that is not set is skipped with a warning:

```text
warning: environment variable 'OPENAI_API_KEY' is not set on this host, so the session starts without it
```

#### `agents[].notify.stopHook`

**boolean.** Default: `false`. For Claude Code: installs a Stop hook in the sandbox so every finished task is announced to hort, which raises a notification on the host. See [Notifications](recipes.md#notifications). An agent without this key never notifies.

### `network`

**array of objects.** Default: none. Merge: combined, keyed by `host` and `port`.

Databases and other TCP services the sandbox should reach. Every entry is reachable inside the sandbox at `127.0.0.1:<port>`, under any egress posture. See [Databases](networking.md#databases).

| Key | Type | Meaning |
| :--- | :--- | :--- |
| `mode` | string, required | `"host"` (a service installed on the host) or `"network"` (a containerized one). Informational; both work the same way. |
| `host` | string, required | The address the service answers on, as seen from your host. |
| `port` | number, required | Its port, and the port the sandbox uses. |

### `egress`

**boolean or object.** Default: open. Merge: replaces entirely.

| Value | Meaning |
| :--- | :--- |
| absent or `true` | Open: the sandbox reaches whatever the host reaches, no proxy. |
| `{ "allow": ["host", "*.domain"] }` | Allowlist: only these hosts, over HTTPS, through a proxy; plus declared `network` entries. |
| `false` | An empty allowlist: no host at all; declared `network` entries still reachable. |

A bare entry matches that exact host; a `*.` entry matches subdomains only, never the apex. An entry that is not a plain host name (a URL, a port) makes `hort up` fail with `invalid name`. See [Networking and egress](networking.md).

### `notifications`

**object.** Default: none. Merge: field by field.

Where and how hort raises a completion announced by an agent with `notify.stopHook`.

| Key | Type | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `sink` | string | `"desktop"` | Where to raise it. `desktop` (through `notify-send`) is the only sink this version has; any other value produces a warning and no notification. |
| `message` | string | `"hort sandbox '<name>' finished"` | The notification text. `<name>` is replaced by the sandbox name. |

### `cache`

**object.** Default: `{}`.

#### `cache.dirs`

**array of strings or objects.** Default: `[]`. Merge: combined.

Writable directories that persist across sandboxes of the same project, for dependency caches such as `node_modules` or a pip cache.

- A **string** `"node_modules"` is mounted at `/workdir/node_modules`.
- An **object** `{ "name": "pip", "target": "~/.cache/pip" }` is mounted at `target`, a path inside the sandbox. A leading `~/` there means the sandbox home, so this lands at `/home/hort/.cache/pip`.

On the host, each lives at `~/.local/state/hort/cache/<project path, encoded>/<name>/`, shared by every sandbox built from the same project and kept when a sandbox goes down. hort creates the directories. Only `hort prune` removes them, and never while a running sandbox uses them.

A cache mounted at `/workdir/<name>` leaves an empty directory of that name in the worktree on the host. git ignores empty directories, so it does not make the worktree dirty.

A cache may target a path inside a read-only mount; it becomes a writable directory there. The path must already exist inside the read-only source on the host, or `hort up` refuses:

```text
cache 'fish' targets '/home/hort/.config/fish', which does not exist inside the read-only mount '/home/you/.config' — point the cache elsewhere, or create it on the host first
```

### `resources`

**object.** Default: none (no ceiling). Merge: field by field.

A per-sandbox ceiling enforced with cgroup v2.

| Key | Type | Meaning |
| :--- | :--- | :--- |
| `memory` | string | Maximum memory. A positive integer with an optional `k`, `m`, `g` or `t` suffix, optionally followed by `b` or `ib`, case-insensitive and base 1024: `4g`, `4G`, `4gb` and `4GiB` all mean 4 GiB. No suffix means bytes. |
| `cpus` | number | CPU time as a number of cores (`2` is two cores' worth, `0.5` half of one). It caps time; it never pins cores. |

A value that does not parse stops `hort up`:

```text
invalid config: resources.memory 'lots' is not a positive size like "4g" (an integer with an optional k, m, g or t suffix, base 1024)
```

If your user does not have the needed cgroup controller delegated, that ceiling is dropped and the sandbox starts anyway:

```text
warning: cgroup controller 'cpu' is not delegated to this user, so the cpu ceiling is not enforced; add 'cpu' to Delegate= in a systemd drop-in for user@.service
```

See [Installation](installation.md#fixing-what-doctor-reports) for enabling delegation.

## `.devcontainer/devcontainer.json`

When a project has no `.hort.json`, hort reads its `devcontainer.json` (also JSONC), but takes very little from it:

- Each entry of `mounts` written in the string form and containing `readonly`, for example `"source=/home/you/.config/nvim,target=/root/.config/nvim,type=bind,readonly"`, adds its `source` to `mounts.readOnly`. The `target` is ignored; the path is placed like any other read-only mount. Variables such as `${localEnv:HOME}` are not expanded.
- `image`, `build`, `features` and `customizations` are ignored, each with a warning printed on every `up`, `attach` and `doctor`:

```text
warning: devcontainer.json 'image' is ignored: hort runs a prepared rootfs and never builds images
```

- Everything else is ignored silently.

The rootfs still has to come from your global configuration. To configure anything else for such a project, add a `.hort.json`, which then replaces the devcontainer file entirely.

## Environment variables

hort itself reads:

| Variable | Effect |
| :--- | :--- |
| `XDG_CONFIG_HOME` | Global configuration under `$XDG_CONFIG_HOME/hort/`. |
| `XDG_STATE_HOME` | Records, worktrees, overlays and caches under `$XDG_STATE_HOME/hort/`. |
| `XDG_RUNTIME_DIR` | Container state, helper pid files and logs under `$XDG_RUNTIME_DIR/hort/`. |
| `SHELL` | The session shell when `shell` is not configured and the rootfs has that path. |
| Names in `agents[].auth.env` | Copied into sessions. |

Every session gets, in addition to your forwarded variables: `HORT_SANDBOX`, `HORT_WORKTREE`, `HOME=/home/hort`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` (all under `/home/hort`), `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, and under an allowlist the proxy variables. Nothing else from your host environment reaches a session; in particular `TERM`, `COLORTERM` and `LANG` do not (see [Colors inside the sandbox](recipes.md#colors-inside-the-sandbox)).
