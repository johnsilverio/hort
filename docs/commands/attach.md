# hort attach

Open one more session in a running sandbox.

```text
hort attach <NAME>

Arguments:
  <NAME>  The sandbox to join

Options:
  -h, --help  Print help
```

## Examples

```bash
hort attach fix-login                     # a new shell in the sandbox
echo 'npm test' | hort attach fix-login   # run commands without a terminal
```

## What it does

`attach` never builds anything. For a running sandbox it opens a **new session**: a login shell, started in `/workdir`, inside the sandbox's namespaces. Any number of sessions can be open at once, and every one sees the same files.

It checks only that the kernel allows user namespaces. It does not need `pasta`, `git` or the rootfs directory, because the sandbox is already running.

**The shell** is the first of: the configured [`shell`](../configuration.md#shell); your host `$SHELL`, if the rootfs has that same path; `/bin/sh`.

**The environment** of the session contains `HORT_SANDBOX` and `HORT_WORKTREE`, `HOME=/home/hort` and the `XDG_*` directories under it, a standard `PATH`, the variables your agents declare in `auth.env`, and, if the sandbox runs an egress proxy, the proxy variables. Nothing else is inherited from your host environment. The configuration is read again at every attach, so a changed `auth.env` or `shell` applies to the next session.

**The terminal.** When hort's standard input is a terminal, the session gets its own terminal allocated inside the sandbox, which hort relays to yours and resizes with your window. Your own terminal is never handed to the sandbox. When standard input is not a terminal (a pipe, a file, a script), the session runs on hort's standard streams instead, which is how you script it.

`attach` records the time, which `hort ls` uses for idle time.

## Exit status

hort exits with the exit status of the session: the shell's own status when it exits, or `128 + signal` when it is killed by a signal. So `hort attach fix-login` in a script tells you what the commands inside did, and an interrupted session is never reported as success.

```console
$ echo 'exit 7' | hort attach fix-login; echo $?
7
```

## Messages

| Message | Meaning and what to do |
| :--- | :--- |
| `no sandbox named '<name>' (run 'hort ls' to see what's alive)` | hort has no record of that name. Check `hort ls`. A sandbox shown as `lost-record` also gets this: hort cannot join a sandbox it has no record of; `hort down <name>` it. |
| `sandbox '<name>' is not running (run 'hort up <name>' to start it, or 'hort prune' to clean up the stale record)` | The sandbox is `orphaned`. `hort up <name>` brings it back on the same worktree. |
| `sandbox '<name>' is running but its container state is gone, so no session can join it (commit what you want to keep from <worktree> on the host, then run 'hort down <name>' and 'hort up <name> --branch <branch>')` | The container runs, but the runtime's bookkeeping for it under `$XDG_RUNTIME_DIR/hort` was deleted, and hort does not rebuild it. Commit from the named worktree on the host, then do what the message says. |
| `sandbox '<name>' is running but its container state is gone, so no session can join it (run 'hort down <name>' and 'hort up <name>')` | The same, for a project without git. `down` leaves your folder alone. |
| `unprivileged user namespaces are disabled in this kernel — hort cannot create a sandbox` | See [Installation](../installation.md#fixing-what-doctor-reports). |
| `invalid config: ...` | A configuration file does not parse. `attach` reads it for `shell` and `auth.env`. |
| `warning: environment variable '<VAR>' is not set on this host, so the session starts without it` | A variable named in `agents[].auth.env` is not set in the terminal you ran `attach` from. `export` it and attach again. |
| `warning: devcontainer.json '<key>' is ignored: hort runs a prepared rootfs and never builds images` | Printed whenever the configuration comes from a devcontainer file. |
| `container runtime failed: ...` | Joining the sandbox failed. See [Troubleshooting](../troubleshooting.md#container-runtime-failed-). |
