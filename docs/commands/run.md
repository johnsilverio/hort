# hort run

Run one command in a running sandbox, with no terminal, and exit with its status.

```text
hort run <NAME> -- <COMMAND>...

Arguments:
  <NAME>        The sandbox to run the command in
  <COMMAND>...  The command and its arguments, taken verbatim after `--`

Options:
  -h, --help  Print help
```

## Examples

```bash
hort run fix-login -- npm test           # run the tests, get their exit code
hort run fix-login -- ruff check .        # lint inside the sandbox
hort run fix-login -- sh -c 'echo hi'     # a shell one-liner
```

## What it does

`run` is `attach` without a terminal. For a running sandbox it opens a **new session** that execs the command you named instead of a login shell, started in `/workdir`, inside the sandbox's namespaces. The command's own standard output and error come back on hort's, so a script or an orchestrator on your host can drive a box command by command and read each result.

Like `attach`, it never builds anything and checks only that the kernel allows user namespaces; it does not need `pasta`, `git` or the rootfs directory, because the sandbox is already running.

**The environment** of the session is the same one `attach` builds: `HORT_SANDBOX` and `HORT_WORKTREE`, `HOME=/home/hort` and the `XDG_*` directories under it, a standard `PATH`, the variables your agents declare in `auth.env`, and, if the sandbox runs an egress proxy, the proxy variables. Nothing else is inherited from your host environment.

**No shell configuration is read.** Because hort starts no shell for it, nothing your rootfs sets in its shell configuration, `/etc/profile.d` for example, exists for the command, including the `IS_SANDBOX=1` that lets Claude Code run unrestricted as root ([Agents that refuse to run as root](../rootfs.md#agents-that-refuse-to-run-as-root)). So `hort run fix-login -- claude --dangerously-skip-permissions` is refused by Claude Code, while the same command wrapped in a login shell starts:

```bash
hort run fix-login -- bash -lc 'claude --dangerously-skip-permissions -p "run the tests"'
```

**No terminal is allocated.** A command that ends is not a shell you type into, so `run` never asks the sandbox for a pty and never lends it yours. The command runs on hort's standard streams.

`run` records the time, the same as `attach`, so a box an orchestrator is driving command by command does not read as idle to `hort prune --idle` between two of them.

## Exit status

hort exits with the exit status of the command: its own status when it exits, or `128 + signal` when it is killed by a signal. So a command that failed inside the box is tellable from hort failing to open the box.

```console
$ hort run fix-login -- sh -c 'exit 7'; echo $?
7
```

## Messages

`run` reaches a live sandbox the same way `attach` does and refuses the same three states with the same messages: a name nothing knows, a sandbox that is not running, and a running sandbox whose container state is gone. See [`hort attach`](attach.md#messages) for each.
