# Concepts

hort has few moving parts. Knowing exactly what each one is makes everything else, from `ls` states to what `down` deletes, predictable.

## Sandbox

A **sandbox** is a pair that lives and dies together:

- a **git worktree on your host**, on its own branch, and
- a **container** that mounts that worktree at `/workdir`.

It has a name you choose (`hort up <name>`), which is how every other command refers to it. It is born when `hort up` builds it and it dies **only** when you run `hort down` (or `hort prune` collects it). No timer ever stops it, however long it sits idle.

A sandbox is born **empty**. It runs no agent. It is a place you go into.

## Anchor

Something has to keep a container alive while nobody is inside. hort starts `sleep infinity` as the container's first process, the **anchor**. It does nothing else. Its process is also how hort knows a sandbox is alive: the kernel's process table, not a file hort wrote, is the truth.

Nothing inside a sandbox can stop the anchor. The kernel discards signals sent to the first process of a pid namespace from inside that namespace, so `kill -9 1` from a session does nothing. Only `hort down` on the host ends a sandbox.

## Session

A **session** is a process that joins a running sandbox: a login shell in `/workdir`. `hort up` opens one (unless you pass `-d`) and every `hort attach` opens another. You can have as many as you like, all seeing the same files.

Sessions are independent of the sandbox. Exiting a shell, closing a terminal or losing an SSH connection ends that session only. The sandbox and every other session carry on, and `hort attach` gets you back in.

hort does not referee between sessions: two agents editing the same file in one sandbox can collide exactly as they would in two terminals on your machine. Every session runs as the same user in the same process namespace, so one session can also kill another's processes. See [Security model](security.md#what-an-agent-inside-can-and-cannot-end).

When a session asks for a terminal, hort gives it a new terminal allocated inside the sandbox and relays it; your own terminal is never handed to the box. `hort attach` and `hort up` exit with the session's exit status, or `128 + signal` if it was killed by a signal, so scripts can tell what happened inside.

## Worktree and branch

In a git repository, `hort up <name>`:

1. creates a **new branch named `<name>`** from the repository's current `HEAD`,
2. checks it out in a new worktree at `~/.local/state/hort/sandboxes/<name>/worktree-<name>`,
3. mounts that worktree at `/workdir`.

Because the worktree is cut from `HEAD`, uncommitted changes in your own checkout are not in it.

With `--branch <existing>` hort checks out a branch that already exists instead of creating one. git allows a branch in only one worktree at a time, so a branch you have checked out elsewhere (including your main checkout) is refused.

Committing in the worktree advances that branch in your real repository, which is the point. Every other branch, and all history not on this branch, stays out of the sandbox's reach.

### Git is a host activity

The worktree's `.git` is a small file pointing at your repository's real `.git` directory by its host path, and that path does not exist inside the sandbox. So **today no git command works inside a sandbox**: not `status`, not `diff`, not `commit`, even if git is installed in the rootfs. This is deliberate. Making git work inside would mean mounting your repository's `.git` into the box writable, and then an unrestricted agent could rewrite history or delete branches in your real repository, instead of being limited to one disposable worktree.

So the division of labour is: **the agent writes files, you commit them from the host.** Review with `git diff` in the worktree directory, commit there, while the sandbox is still running. `$HORT_WORKTREE`, set in every session, holds the host path of that directory.

**Planned, not available yet: an opt-in clone mode.** In it, a sandbox would get its own clone of the repository, so an agent could commit, branch and open a pull request with its own tools, while your host repository is never written from inside. Pushing to a remote such as GitHub would use a narrowly scoped token you choose to pass in, so what the agent can do there is bounded by that token and by the remote's branch protection. On large repositories a clone costs noticeably more disk space and time than a worktree, which is why it will be opt-in. None of this exists in hort today, and there is nothing to configure for it yet. See the [Roadmap](roadmap.md#clone-mode-opt-in).

### What `down` keeps

`hort down <name>` removes, in this order: the host-side helpers and every process in the box, the container, the worktree directory (with any uncommitted changes), and hort's record. It **keeps**:

- the branch `<name>` and every commit on it,
- your repository and every other branch,
- the project's [dependency caches](configuration.md#cache).

Anything not committed when you run `down` is gone. Commit first.

Because the branch stays, `hort up <name>` later finds it already there. On a terminal, hort offers to build the sandbox on that branch; without a terminal it refuses and prints the command that does it (`hort up <name> --branch <name>`). Delete a branch you no longer need with `git branch -d <name>` on the host (a flag to do it from `down` is [planned](roadmap.md#deleting-the-sandboxs-branch-on-down-and-prune)).

## Projects, and the mode without git

hort only builds a sandbox for a **project**. Starting from the directory you run it in, it walks up to the nearest directory that holds `.hort.json`, `.devcontainer/devcontainer.json` or `.git`. That directory is the project: its `.hort.json` is the project configuration. If there is none anywhere up the chain, `hort up` refuses, so that running it by accident in your downloads folder never mounts that folder into a box.

If the project is inside a git repository, you get the worktree and branch described above. If it is not (a folder marked only by a `.hort.json` or a devcontainer file), hort mounts **the project folder itself** at `/workdir`:

- there is no branch and no worktree, so `ls` shows `-` for branch and dirty state;
- the agent writes directly into your folder;
- `--branch` is refused;
- `hort down` removes the container and the record, and **never** your folder.

You keep container isolation but lose the disposable draft. Prefer a git repository for anything you care about.

## States in `hort ls`

hort never trusts its own records about what is running. Every `ls` compares three sources, its records on disk, the processes the kernel is running and the worktrees on disk, and reports what it finds.

| State | What it means | What to do |
| :--- | :--- | :--- |
| `live` | The anchor is running and the worktree is on disk. | Nothing. `hort attach <name>` to go in. |
| `orphaned` | hort has a record, but the anchor is gone: the machine rebooted, or the process was killed. The worktree and its uncommitted work are still on disk. | `hort up <name>` brings it back on the same worktree, work intact. `hort down <name>` discards it. `hort prune` would also collect it. |
| `inconsistent` | The anchor is running but the worktree directory was deleted on the host. | `hort down <name>`. What was in the worktree is gone. |
| `lost-record` | A sandbox is running under that name but hort's record of it is gone (for example the state directory was deleted). | `hort down <name>`, which `ls` prints under the row. It stops the container and its helpers; see [Troubleshooting](troubleshooting.md#a-lost-record-sandbox) for the worktree it cannot name. |

A `live` sandbox can also be **half-built**: the anchor runs but its networking is not standing, because the `hort up` that built it was interrupted or a helper process died. `ls` still says `live`. Run `hort up <name>` again: it rebuilds the network into the running sandbox without touching the sessions inside.

## Idle and age

`age` is the time since `hort up` built the sandbox. `idle` is `active` while any process besides the anchor runs inside; otherwise it is the time since the most recent of: creation, the last `hort attach`, and the last completion an agent announced through [notifications](recipes.md#notifications). hort does not scan the worktree for changes, so an agent quietly editing files in a session that is still open simply shows as `active`.

Idle time only informs you, and [`hort prune --idle`](commands/prune.md) if you ask for it. hort never stops a sandbox because it is idle: stopping one by mistake destroys uncommitted work, and leaving it running only costs memory.

## Where things live

| Path | What |
| :--- | :--- |
| `~/.config/hort/config.json` | Global configuration. |
| `<project>/.hort.json` | Project configuration (or `.devcontainer/devcontainer.json`). |
| `~/.local/state/hort/sandboxes/<name>/metadata.json` | hort's record of the sandbox. |
| `~/.local/state/hort/sandboxes/<name>/worktree-<name>/` | The worktree (git mode). |
| `~/.local/state/hort/sandboxes/<name>/overlay/` | The sandbox's disposable writable layer over the rootfs. |
| `~/.local/state/hort/sandboxes/<name>/notify/` | The notification channel, when configured. |
| `~/.local/state/hort/cache/<encoded project path>/` | The project's dependency caches, shared by all its sandboxes. |
| `$XDG_RUNTIME_DIR/hort/sandboxes/<name>/output.log` | Log of the host-side helpers, including egress proxy decisions. |

`XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR` move these the usual way. Everything under the runtime directory is discarded on reboot, which is exactly what a reboot does to running sandboxes.
