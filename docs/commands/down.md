# hort down

Tear a sandbox down.

```text
hort down [OPTIONS] <NAME>

Arguments:
  <NAME>  The sandbox to tear down

Options:
  -f, --force  Skip the open-sessions confirmation
  -h, --help   Print help
```

## Examples

```bash
hort down fix-login      # asks first if sessions are open
hort down fix-login -f   # no question
```

## What it does

Always in this order, because deleting a directory that a running process still has mounted corrupts I/O:

1. Stops the notification watcher and the host-side network helpers (pasta, the egress proxy, database forwarders).
2. Removes the container, which ends every session and the anchor and releases the worktree mount.
3. In a git project, **removes the worktree directory, including uncommitted changes**, and its registration in your repository.
4. Removes hort's record of the sandbox.

It **keeps** the branch and its commits (deleting it from `down` is [planned](../roadmap.md#deleting-the-sandboxs-branch-on-down-and-prune)), your repository, and the project's dependency caches. In a project without git it never touches your folder: only the container and the record go.

`down` prints nothing when it succeeds.

Naming the sandbox is taken as your decision: `down` does not check for uncommitted work. Commit first. The only question it asks is about open sessions.

## Open sessions

If processes are running in the sandbox besides its anchor (someone, or some agent, is inside), `down` asks on a terminal:

```text
tear down sandbox 'fix-login' with open sessions? [y/N]
```

Only `y` or `yes` proceeds; anything else, including Enter, leaves the sandbox alone and exits `0`. `-f` skips the question. Without a terminal and without `-f`, `down` refuses instead of guessing:

```text
refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)
```

## Sandboxes in other states

- **`orphaned`**: there is nothing running to stop; `down` removes the worktree and the record.
- **`inconsistent`** (worktree deleted on the host): `down` stops the container and clears the stale worktree registration and the record.
- **`lost-record`**: hort has no record, but the container is running. `down` stops the helpers and the container and stops there, because the worktree's path was in the missing record. See [Troubleshooting](../troubleshooting.md#a-lost-record-sandbox) for what may be left on disk.

## Messages

| Message | Meaning and what to do |
| :--- | :--- |
| `no sandbox named '<name>' (run 'hort ls' to see what exists)` | Neither a record nor a running container has that name. |
| `refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)` | Sessions are open and there is nobody to ask. Add `-f` if you mean it. |
| `container runtime failed: ...` / `sandbox networking failed: ...` | A step failed. hort stops at the failing step, so later steps (worktree, record) have not run; run `hort down <name>` again, and see [Troubleshooting](../troubleshooting.md). |
| `git command failed: ...` | Removing the worktree failed; git's message says why. |
