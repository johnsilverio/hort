# Quickstart

This walks through one full loop: build a sandbox for a task, run an agent in it, review and commit its work from the host, and tear the sandbox down. It assumes you have [installed hort](installation.md) and [prepared a rootfs](rootfs.md) at `~/.local/share/hort/devbox`.

The example repository is `~/src/webapp` and the task is called `fix-login`.

## 1. Point hort at your rootfs

The first time you run `hort up` on a terminal with no global configuration, hort opens a short setup dialogue, [`hort config`](commands/config.md), and writes `~/.config/hort/config.json` for you. You can also write the file by hand. The only field hort cannot work out for itself is `rootfs`:

```jsonc
// ~/.config/hort/config.json
{
  "rootfs": "~/.local/share/hort/devbox",
}
```

Check it:

```console
$ hort doctor
...
configuration
  rootfs               ready
```

## 2. Commit what the agent should start from

A sandbox's worktree is cut from the repository's current `HEAD`. **Uncommitted changes in your checkout do not come along.** Commit (or stash) first:

```bash
cd ~/src/webapp
git status
git commit -am "wip before handing off"
```

## 3. Build the sandbox

```bash
hort up fix-login
```

hort has created a branch `fix-login` from `HEAD`, checked it out in a new worktree, started the container and opened a login shell inside it. What the prompt looks like is up to the shell configuration in your rootfs; the hostname is the sandbox name. You start in `/workdir`, which is the worktree:

```bash
pwd                 # /workdir
echo $HORT_SANDBOX  # fix-login
echo $HOME          # /home/hort, a home that lives in memory
```

Your host home does not exist in here. Nothing outside the worktree, the caches you declare and the read-only paths you configure is reachable from inside.

If you would rather get your prompt back and enter later, use `hort up fix-login -d`, then `hort attach fix-login`.

## 4. Run an agent

Start the agent the way you always do. The sandbox never starts one for you.

```bash
claude --dangerously-skip-permissions
```

The agent can write anywhere in `/workdir`, install packages, and delete whatever it likes. The worst it can do is wreck this worktree. For Claude Code to accept that flag inside a sandbox, your rootfs needs `IS_SANDBOX=1` (see [Agents that refuse to run as root](rootfs.md#agents-that-refuse-to-run-as-root)), and its credentials have to be mounted; the recipe is in [Configuration recipes](recipes.md#agent-credentials).

## 5. Open more sessions

From another terminal, or another tmux pane:

```bash
hort attach fix-login
```

Each `attach` is a new shell in the same sandbox, seeing the same files. Run a second agent there, a test watcher, an editor. Closing a session never stops the sandbox; exiting the shell `hort up` opened doesn't either.

## 6. See what is running

```console
$ hort ls
fix-login  live  2  41m 3s 211ms 802us 45ns  active  fix-login  dirty
```

The columns are name, state, sessions (processes running in the box besides its idle anchor), age, idle time (`active` while anything runs), branch, and whether the worktree has uncommitted changes. See [hort ls](commands/ls.md).

## 7. Review and commit from the host

Git does not work inside the sandbox (see [why](concepts.md#git-is-a-host-activity)). You review and commit on the host, against the worktree, while the sandbox is still running. The worktree lives at `~/.local/state/hort/sandboxes/<name>/worktree-<name>`; inside the box, `$HORT_WORKTREE` prints that host path too.

```bash
cd ~/.local/state/hort/sandboxes/fix-login/worktree-fix-login
git status
git diff
git add -A
git commit -m "Fix login redirect loop"
```

That commit lands on the branch `fix-login` in your real repository. Merge it however you normally do:

```bash
cd ~/src/webapp
git merge fix-login
```

Not happy with something? Tell the agent and let it keep working; the sandbox is still there.

## 8. Tear it down

```console
$ hort down fix-login
tear down sandbox 'fix-login' with open sessions? [y/N] y
```

`down` asks only when sessions are still open (`-f` skips the question). It stops everything in the box, removes the container, **deletes the worktree with any uncommitted changes**, and removes hort's record. It prints nothing when it succeeds.

It keeps the **branch** and every commit on it. If you run `hort up fix-login` again later, hort notices the branch and offers to build on it:

```console
$ hort up fix-login
branch 'fix-login' already exists (a 'hort down' keeps a sandbox's branch) — build sandbox 'fix-login' on it? [Y/n]
```

Once the branch is merged and you no longer need it, delete it on the host with `git branch -d fix-login`.

## Where next

- [Concepts](concepts.md): what exactly a sandbox, a session and a worktree are, and what each `ls` state means.
- [Running agents in parallel](parallel-agents.md): several agents, several sandboxes, with tmux.
- [Configuration recipes](recipes.md): dotfiles, credentials, caches, databases, an egress allowlist, notifications.
