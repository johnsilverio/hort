# Troubleshooting

Find the message you see, or the symptom, below. For refusals of a specific command, each [command page](commands/index.md) lists every message it can print. Start any investigation with:

```bash
hort doctor   # what the host can do
hort ls       # what hort thinks exists
```

and, for anything about networking or notifications, the sandbox's host-side log:

```bash
cat "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/hort/sandboxes/<name>/output.log"
```

## Setting up

### `no rootfs configured — set "rootfs" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json`

No configuration layer names a rootfs. [Prepare one](rootfs.md), then set `rootfs` in `~/.config/hort/config.json`. On a terminal, `hort config` writes the file for you.

### `rootfs '<path>': /workdir is not writable by the mapped uid — make it world-writable`

Run `chmod 1777 <path>/workdir`. Exporting an image into a directory as a normal user resets that mode.

### `rootfs '<path>' has no usable shell (expected /bin/sh) — the rootfs must provide one`

The directory is not a root filesystem, or not the one you meant: check that `<path>/bin/sh` exists. A common cause is exporting into a subdirectory.

### `'<path>' is not a project — run hort from a git repository, or add a .hort.json there to sandbox the directory itself`

You ran `hort up` outside any repository or marked folder. `cd` into your project. To sandbox a plain folder, create a `.hort.json` in it (`{}` is enough).

### `unprivileged user namespaces are disabled in this kernel — hort cannot create a sandbox`

See [Installation](installation.md#fixing-what-doctor-reports).

### `two databases are declared on port <port> (<host> and <other>), and a sandbox can reach only one of them — remove one from "network" in your configuration or give it another port`

Inside a sandbox every declared database is `127.0.0.1:<port>`, so two on one port collide. Remove one, or give it another port. Nothing was built or changed; a sandbox that is already running is unaffected.

### `git command failed: worktree add: fatal: invalid reference: HEAD`

The repository has no commit yet, so there is nothing to branch from. Make a first commit.

## Building and entering sandboxes

### `branch '<name>' already exists (a 'hort down' keeps a sandbox's branch) — run 'hort up <name> --branch <name>' to build the sandbox on it, or choose another name`

A previous sandbox of this name was torn down and its branch kept. Run the command in the message to continue on that branch (on a terminal, hort offers it), or pick a new name. Delete the branch with `git branch -d <name>` if you no longer need it.

### `branch '<branch>' is already checked out in another worktree`

git allows a branch in one worktree at a time. If it is checked out in your main checkout, switch that checkout to another branch first (`git switch main`). `git worktree list` shows where it is.

### `a sandbox named '<name>' already exists (run 'hort attach <name>' to join it, or 'hort down <name>' first)`

A sandbox of that name is running. If you want a fresh one, commit what you need and `hort down <name>` first. If `hort ls` shows it as `inconsistent` or `lost-record`, `hort down <name>` is the only way forward.

### `another 'hort up <name>' is already in progress`

Another `hort up` of the same name is running, possibly waiting at a prompt in another terminal. Finish or cancel it. A killed `hort up` releases the lock automatically.

### `sandbox '<name>' is not running (run 'hort up <name>' to start it, or 'hort prune' to clean up the stale record)`

The sandbox is `orphaned`: its container is gone, usually because the machine rebooted. The worktree and its uncommitted changes are still on disk. `hort up <name>` rebuilds the container on the same worktree.

### `sandbox '<name>' is running but its container state is gone, so no session can join it (...)`

Something deleted hort's runtime files under `$XDG_RUNTIME_DIR/hort/containers/` while the sandbox ran. hort does not reconstruct them. Commit what you want to keep from the worktree named in the message, then run the two commands the message gives.

### The changes I had in my checkout are not in the sandbox

A worktree is cut from `HEAD`. Uncommitted changes stay in your checkout. Commit them, then `hort down` and `hort up` again, or build the sandbox on a branch that has them with `--branch`.

### `container runtime failed: ...`

Creating, joining or removing the container failed; the rest of the message comes from the runtime and names the step. Check, in order:

1. `hort doctor`: user namespaces, rootless overlayfs.
2. The rootfs: is it a complete root filesystem, and did something change it while sandboxes were using it? Never modify a rootfs a live sandbox uses; see [Changing a rootfs](rootfs.md#changing-a-rootfs).
3. `hort ls`: if the sandbox shows as `orphaned` after a failed build, `hort up <name>` retries; `hort down <name>` gives up and cleans.

If a build fails after its container started, hort stops what it started and keeps the worktree and the record, so nothing is lost and a retry picks up where it left.

### `sandbox networking failed: ...`

Starting or stopping pasta, the egress proxy or a database forwarder failed. The sandbox's `output.log` holds pasta's own report. Common details:

- **`binding 127.0.0.1:<port> for <host>: Address already in use`**: a declared database on another address needs a forwarder on your host's `127.0.0.1:<port>`, and something already listens there. Free the port, or use the service that is already there by declaring `"host": "127.0.0.1"`.

After fixing the cause, `hort up <name>` completes the sandbox.

## Inside the sandbox

### `--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons`

Claude Code sees uid 0. In a hort sandbox that is the root of an unprivileged user namespace with no capabilities, which is safe. Set `IS_SANDBOX=1` in the rootfs; see [Agents that refuse to run as root](rootfs.md#agents-that-refuse-to-run-as-root).

### git says the directory is not a repository

Expected: today git does not work inside a sandbox, by design, because the worktree's `.git` points at your host repository, which the box does not have. Run git on the host, in the worktree (`~/.local/state/hort/sandboxes/<name>/worktree-<name>`, also printed by `echo $HORT_WORKTREE` inside). An opt-in clone mode that would let an agent commit inside is [planned but not available yet](roadmap.md#clone-mode-opt-in). See [Git is a host activity](concepts.md#git-is-a-host-activity).

### `error: Unable to open universal variable file '/home/hort/.config/fish/fish_variables': EROFS: Read-only file system`

`~/.config/fish` is mounted read-only as a whole. Mount its parts instead; see [the fish tip](recipes.md#fish-and-a-read-only-config-directory).

### Programs show no colors

Sessions do not inherit `TERM` or `COLORTERM`. See [Colors inside the sandbox](recipes.md#colors-inside-the-sandbox).

### A tool cannot reach the network (egress allowlist)

Look at the proxy's decisions in `output.log`:

- `refused <host> (not in the allowlist)`: add the host to `egress.allow` if you trust it, then `hort down` and `hort up`.
- `refused <url> (only CONNECT tunnels are proxied)`: the tool made a plain HTTP request. Use `https://`. For a local address, the tool is ignoring `NO_PROXY`.
- no line for that host: the tool ignores the proxy variables and has no route. Configure its proxy explicitly, or use a proxy-aware tool.

DNS lookups fail by design under an allowlist; tools resolve through the proxy. See [Networking and egress](networking.md).

### A tool cannot resolve names (open egress)

The sandbox's `/etc/resolv.conf` should say `nameserver 198.51.100.53`. If a program overwrote it, restore that line. If it is right, check that name resolution works on the host.

### A database is unreachable

Inside the sandbox, use `127.0.0.1:<port>`, never the host's own address. Under an allowlist, the database must be declared in `network`; declaring it later needs `hort down` and `hort up`. See [Databases](networking.md#databases).

### My shell session disappeared

A session ends when its shell exits or is killed, and an agent can end sessions (see [Security model](security.md#what-an-agent-inside-can-and-cannot-end)). The sandbox is still running: `hort attach <name>`.

### `warning: environment variable '<VAR>' is not set on this host, so the session starts without it`

`export` the variable in the terminal you run `hort attach` from, then open a new session.

## Notifications

### No notification when the agent finishes

1. `hort up` printed a warning about `notify-send` or the sink? Fix that, then rebuild the sandbox.
2. The Claude Code entry has `"notify": { "stopHook": true }`, and the sandbox was built after you added it? Check that `/etc/claude-code/managed-settings.d/hort-notify.json` exists inside.
3. Append a test line from a session: `echo '{}' >> /run/hort/notify/events.jsonl`. If nothing appears, read `output.log`: a line starting with `hort: a completion of this sandbox was not raised:` means `notify-send` ran and failed, typically because there is no desktop session (for example over SSH).

## State that looks wrong

### A sandbox is `orphaned` after a reboot

Normal. A reboot stops every sandbox's processes; hort's records and your worktrees remain. `hort up <name>` brings a sandbox back with its uncommitted work, or `hort down <name>` discards it.

### A sandbox is `inconsistent`

Its worktree directory was deleted on the host while it ran. Anything uncommitted in it is gone. `hort down <name>` removes the container and the stale registration.

### A lost-record sandbox

`hort ls` shows `lost-record` when a sandbox is running but hort's record of it is gone, for example because `~/.local/state/hort/sandboxes/<name>` was deleted. `attach` and `up` cannot use it. Run the command `ls` prints under the row:

```bash
hort down <name>
```

It stops the container and its helpers. It cannot remove the worktree, because its path was in the lost record: if the directory `~/.local/state/hort/sandboxes/<name>/worktree-<name>` still exists, commit anything you need from it on the host, then run `git worktree remove --force <that path>` in the repository it belonged to (or delete the directory and run `git worktree prune`). Adopting such a sandbox back into hort is not supported yet; it is on the [Roadmap](roadmap.md#adopting-a-sandbox-whose-record-was-lost).

### `hort prune` ends with `git command failed: worktree prune: fatal: not a git repository ...`

Run `hort prune` from inside a git repository; its last step needs one.
