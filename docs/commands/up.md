# hort up

Build a sandbox and open a session in it.

```text
hort up [OPTIONS] <NAME>

Arguments:
  <NAME>  The sandbox to build, which is also the branch it creates

Options:
      --branch <BRANCH>  Check out this existing branch instead of creating one named after the sandbox
      --git <GIT>        How the sandbox gets git into `/workdir`, overriding what the configuration declares: `worktree` or `clone`
  -d, --detach           Return to the prompt with the sandbox running instead of opening a session in it
  -h, --help             Print help
```

## Examples

```bash
cd ~/src/webapp
hort up fix-login                      # new branch fix-login from HEAD, then a shell inside
hort up fix-login -d                   # same, but return to your prompt
hort up review --branch feature/search # build on an existing branch
hort up ship-it --git clone            # give the box its own clone, so git works inside
```

## What it does

1. **Reads the configuration** (global, then project; see [Configuration](../configuration.md)). On a terminal with no global configuration yet, it first runs the [`hort config`](config.md) dialogue.
2. **Checks that there is a project here**: the nearest directory up from where you are that holds `.hort.json`, `.devcontainer/devcontainer.json` or `.git`.
3. **Checks the host and the rootfs**, in this order: user namespaces, `pasta`, `git`, `ip` (only for an egress allowlist), then the rootfs: configured, present, has `/bin/sh`, has the configured `shell`, has a world-writable `/workdir`.
4. **Checks the rest of the configuration** that can be wrong: resource sizes, databases sharing a port, and caches aimed inside read-only mounts.
5. **Takes a per-name lock**, so two `hort up` of the same name cannot race.
6. **Decides what to build.** A sandbox of that name that is already fully running is refused. One that is half-built (an orphan, or a live box whose networking is gone) is completed instead of refused.
7. **Prepares `/workdir`**: in a git repository, a new branch `<name>` from `HEAD` (or the `--branch` you named) checked out at `~/.local/state/hort/sandboxes/<name>/worktree-<name>`, as a worktree or, in [clone mode](../git-modes.md), as a clone of the repository. Without git, the project folder itself.
8. **Records the sandbox on disk**, before anything starts, so an interruption from here on leaves something `hort ls` can see.
9. **Starts the container** with its anchor process, then **its networking** (pasta; the egress proxy for an allowlist; database forwarders).
10. **Starts the notification watcher**, if an agent declares `notify.stopHook`. If that fails, it warns and carries on.
11. **Opens a session**, unless `-d`: exactly what `hort attach <name>` does.

If starting the container or its networking fails after the container is up, `up` undoes what it started (helpers, then container) and keeps the worktree and the record. The sandbox then shows as `orphaned`, and running `hort up <name>` again retries.

`hort up <name>` without `-d` behaves exactly like `hort up <name> -d` followed by `hort attach <name>`, including its [exit status](attach.md#exit-status).

## Building on a branch a `down` kept

`hort down` keeps the sandbox's branch. When you run `hort up <name>` and a branch `<name>` already exists and is not checked out anywhere, hort asks on a terminal:

```text
branch 'fix-login' already exists (a 'hort down' keeps a sandbox's branch) — build sandbox 'fix-login' on it? [Y/n]
```

Enter (yes) builds the sandbox on that branch, exactly like `--branch fix-login`. Answering no, or running without a terminal, refuses with the ready command:

```text
branch 'fix-login' already exists (a 'hort down' keeps a sandbox's branch) — run 'hort up fix-login --branch fix-login' to build the sandbox on it, or choose another name
```

## Resuming a half-built sandbox

If a sandbox of that name is `orphaned` (its container died, for example after a reboot), `hort up <name>` builds a new container over the **same worktree**, with your uncommitted work intact, on whatever branch the worktree currently holds. If it is `live` but its networking is gone, `hort up <name>` restarts the networking without touching the container or the sessions inside. Neither prints anything on success.

## Messages

### Refusals before anything is built

| Message | Meaning and what to do |
| :--- | :--- |
| `'<path>' is not a project — run hort from a git repository, or add a .hort.json there to sandbox the directory itself` | Neither this directory nor any above it has `.git`, `.hort.json` or `.devcontainer/devcontainer.json`. `cd` into your repository, or add a `.hort.json` (even `{}`) to the folder you want sandboxed. |
| `unprivileged user namespaces are disabled in this kernel — hort cannot create a sandbox` | The kernel refuses unprivileged user namespaces. See [Installation](../installation.md#fixing-what-doctor-reports). |
| `pasta not found on PATH — hort needs it for sandbox networking` | Install `passt`. |
| `git not found on PATH — hort needs it to prepare the sandbox worktree` | Install git. It is needed even for projects that are not repositories. |
| `ip not found on PATH — hort needs iproute2 for allowlist egress` | This project has an egress allowlist. Install `iproute2`. |
| `no rootfs configured — set "rootfs" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json` | No layer sets `rootfs`. See [Preparing a rootfs](../rootfs.md). |
| `rootfs directory '<path>' does not exist — prepare it first with podman export, debootstrap or umoci unpack` | The configured path is not a directory. Check the path, or prepare the rootfs. |
| `rootfs '<path>' has no usable shell (expected /bin/sh) — the rootfs must provide one` | The rootfs lacks `/bin/sh`. Often the export went into the wrong directory. |
| `shell '<shell>' not found in rootfs '<path>' — set "shell" to one the rootfs provides, or omit it` | Your `shell` setting names a path the rootfs does not have. |
| `rootfs '<path>': /workdir is not writable by the mapped uid — make it world-writable` | `chmod 1777 <path>/workdir`. Extracting an export as a normal user drops that mode. |
| `invalid config: <file>: <parser message>` | A configuration file is not valid JSONC. The line and column are in the parser message. |
| `invalid config: resources.memory '<value>' is not a positive size like "4g" (an integer with an optional k, m, g or t suffix, base 1024)` | Fix `resources.memory`. |
| `invalid config: resources.cpus <value> is not a positive number of cores` | Fix `resources.cpus`. |
| `invalid name` | The sandbox name is empty or contains `/`, or an `egress.allow` entry is not a plain host name. |
| `cache '<name>' targets '<target>', which does not exist inside the read-only mount '<source>' — point the cache elsewhere, or create it on the host first` | A `cache.dirs` target falls inside a read-only mount whose host directory lacks that path. Create it on the host, or move the cache. |
| `two databases are declared on port <port> (<host> and <other>), and a sandbox can reach only one of them — remove one from "network" in your configuration or give it another port` | Every declared database is `127.0.0.1:<port>` inside, so two on one port cannot both be reached. Remove one or change a port. |
| `--branch requires a git repository, but this project is not one` | `--branch` makes no sense for a project folder without git. |
| `--git clone requires a git repository, but this project is not one` | There is nothing to clone in a project folder without git. The same mode set in a configuration file only warns and builds the sandbox without git. See [Git inside the sandbox](../git-modes.md#turning-it-on). |
| `another 'hort up <name>' is already in progress` | Another `hort up` of this name holds the lock. Wait for it. The lock is released automatically if that process dies. |

### Refusals about the name and the branch

| Message | Meaning and what to do |
| :--- | :--- |
| `a sandbox named '<name>' already exists (run 'hort attach <name>' to join it, or 'hort down <name>' first)` | A sandbox with this name is already running. Also printed for a name `ls` shows as `inconsistent` or `lost-record`; for those, `hort down <name>` first. |
| `branch '<name>' already exists (a 'hort down' keeps a sandbox's branch) — run 'hort up <name> --branch <name>' to build the sandbox on it, or choose another name` | See [above](#building-on-a-branch-a-down-kept). |
| `branch '<branch>' is already checked out in another worktree` | git allows a branch in one worktree at a time. Switch that other checkout to a different branch, or pick another branch or name. |
| `branch '<branch>' does not exist; create it first or omit --branch to create a new branch named '<name>'` | `--branch` names a branch that does not exist. |
| `sandbox '<name>' holds branch '<held>', not '<requested>' — run 'hort down <name>' first, then 'hort up <name> --branch <requested>'` | You are resuming a half-built sandbox, whose worktree is on another branch than the `--branch` you gave. Commit what you need from its worktree first. |
| `sandbox '<name>' was built in <built> git mode, not <requested> — run 'hort down <name>' first, or repeat it with 'hort up <name> --git <built>'` | You are resuming a half-built sandbox that was built in the other [git mode](../git-modes.md). Finish it in the mode it has, or tear it down and build again. |

### Failures while building

| Message | What to do |
| :--- | :--- |
| `git command failed: <step>: <git's message>` | A git step of the build failed, and git's message says why; the step is named. For `worktree add` the usual cause is a repository with no commit yet (`HEAD` is empty), so make a first commit. |
| `container runtime failed: ...` | See [Troubleshooting](../troubleshooting.md#container-runtime-failed-). |
| `sandbox networking failed: ...` | See [Troubleshooting](../troubleshooting.md#sandbox-networking-failed-). |

### Warnings (the sandbox still starts)

| Warning | Meaning |
| :--- | :--- |
| `warning: cgroup controller '<controller>' is not delegated to this user, so the <controller> ceiling is not enforced; add '<controller>' to Delegate= in a systemd drop-in for user@.service` | Part of `resources` is not applied. |
| `warning: this kernel cannot restrict which ports a process connects to, so the egress allowlist of this sandbox runs without its kernel layer (Linux 6.7 or newer enforces it)` | The allowlist holds, minus its Landlock layer. |
| `warning: the configured 'clone' git mode needs a git repository and this project is not one, so the sandbox mounts the project folder itself` | Your configuration asks for [clone mode](../git-modes.md) and this project is not a repository, so the sandbox is built the way a project without git always is. |
| `warning: read-only mount '<path>' is not on this host, so the sandbox starts without it` | A `mounts.readOnly` or `auth.readOnly` path does not exist. |
| `warning: notify-send is not on PATH, so no completion of this sandbox will be raised on the desktop (install libnotify to get it)` | Notifications are configured but cannot be shown. |
| `warning: this build raises a completion on the desktop and nowhere else, so nothing will be raised on the '<sink>' this configuration asks for` | `notifications.sink` names something other than `desktop`. |
| `warning: no completion of this sandbox will be raised, because its watcher could not be started: ...` | The notification watcher failed to start. |
| `warning: devcontainer.json '<key>' is ignored: hort runs a prepared rootfs and never builds images` | The project is configured by a devcontainer file; that key has no meaning for hort. |

When a session opens (without `-d`), the [warnings of `hort attach`](attach.md#messages) can follow.
