# hort down

Tear a sandbox down.

```text
hort down [OPTIONS] <NAME>

Arguments:
  <NAME>  The sandbox to tear down

Options:
  -f, --force  Skip the confirmations for open sessions and unreturned work
  -h, --help   Print help
```

## Examples

```bash
hort down fix-login      # asks first if sessions are open, or if a clone holds work you never sent out
hort down fix-login -f   # no questions
```

## What it does

Always in this order, because deleting a directory that a running process still has mounted corrupts I/O:

1. Stops the notification watcher and the host-side network helpers (pasta, the egress proxy, database forwarders).
2. Removes the container, which ends every session and the anchor and releases the worktree mount.
3. In a git project, **removes the worktree directory, including uncommitted changes**, and its registration in your repository. In [clone mode](../git-modes.md) that directory is the sandbox's clone, so every commit made inside it that was not pushed or fetched out goes with it, which is why `down` asks first (see [Unreturned work](#unreturned-work)). That step also deletes the sandbox's pinned ref `refs/hort/<name>/base` from your repository; the pins of your other clone-mode sandboxes are left alone.
4. Removes hort's record of the sandbox.

It **keeps** the branch and its commits (deleting it from `down` is [planned](../roadmap.md#deleting-the-sandboxs-branch-on-down-and-prune)), your repository, and the project's dependency caches. In a project without git it never touches your folder: only the container and the record go.

`down` prints nothing when it succeeds.

Naming the sandbox is taken as your decision: `down` does not check for uncommitted work. Commit first. It asks two questions, and only two: about open sessions, and about commits that exist only inside a clone.

## Open sessions

If processes are running in the sandbox besides its anchor (someone, or some agent, is inside), `down` asks on a terminal:

```text
tear down sandbox 'fix-login' with open sessions? [y/N]
```

Only `y` or `yes` proceeds; anything else, including Enter, leaves the sandbox alone and exits `0`. `-f` skips the question. Without a terminal and without `-f`, `down` refuses instead of guessing:

```text
refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)
```

## Unreturned work

In [clone mode](../git-modes.md) the sandbox's `/workdir` is a repository of its own, so a commit made inside it lives nowhere else until it is pushed or fetched out. Before removing such a sandbox, `down` compares the clone's tip against the repository of the project the sandbox was built from, whatever directory you run `down` in, and asks on a terminal when that repository does not have it:

```text
sandbox 'fix-login' holds commits the project repository does not have; tear it down anyway? [y/N]
```

Both reads happen on the host and neither needs the box running. Answering anything but `y` or `yes` leaves the sandbox alone and exits `0`; `-f` skips the question; without a terminal and without `-f`, `down` refuses with the same message as above. If hort cannot read the clone at all, or its record does not say which project it was built from, it asks anyway, because the alternative is deleting commits it could not see. [`hort ls`](ls.md) shows the same answer ahead of time, as `clone, work only in the box`.

Worktree-mode sandboxes are never asked about: their commits go straight into your repository, so tearing the box down does not touch them.

## Sandboxes in other states

- **`orphaned`**: there is nothing running to stop; `down` removes the worktree and the record.
- **`inconsistent`** (worktree deleted on the host): `down` stops the container and clears the stale worktree registration and the record.
- **`lost-record`**: hort has no record, but the container is running. `down` stops the helpers and the container and stops there, because the worktree's path was in the missing record. See [Troubleshooting](../troubleshooting.md#a-lost-record-sandbox) for what may be left on disk.

## Messages

| Message | Meaning and what to do |
| :--- | :--- |
| `no sandbox named '<name>' (run 'hort ls' to see what exists)` | Neither a record nor a running container has that name. |
| `refusing to down without confirmation: stdin is not a TTY (pass --force to proceed)` | Sessions are open, or a clone holds commits your repository does not have, and there is nobody to ask. Add `-f` if you mean it. |
| `container runtime failed: ...` / `sandbox networking failed: ...` | A step failed. hort stops at the failing step, so later steps (worktree, record) have not run; run `hort down <name>` again, and see [Troubleshooting](../troubleshooting.md). |
| `git command failed: ...` | Removing the worktree failed; git's message says why. |

In [clone mode](../git-modes.md), `down` also deletes that sandbox's pinned ref `refs/hort/<name>/base` from your repository, so the commit it was holding for the clone becomes collectable by `git gc` again. Pins belonging to your other clone-mode sandboxes are untouched: each one keeps your repository's gc off the objects that clone is still borrowing.
