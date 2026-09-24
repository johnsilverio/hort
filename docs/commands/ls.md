# hort ls

List every sandbox on this machine with its state.

```text
hort ls

Options:
  -h, --help  Print help
```

## Output

A header naming each column, then one line per sandbox, from every project:

```console
$ hort ls
NAME       STATE        SESSIONS  AGE     IDLE    GIT                          BRANCH     DIRTY
old-spike  orphaned     0         2d 3h   2d 2h   worktree                     old-spike  clean
notes      live         0         35s     15s     -                            -          -
pr-bot     live         1         2h 5m   active  clone, work only in the box  pr-bot     -
fix-login  live         2         41m 3s  active  worktree                     fix-login  dirty
ghost      lost-record  0         -       -       -                            -          -
    running with no record on disk; run 'hort down ghost' to stop its container and host-side helpers
```

Each column is aligned under its name in the header:

| Column | Meaning |
| :--- | :--- |
| `NAME` | The sandbox name. |
| `STATE` | `live`, `orphaned`, `inconsistent` or `lost-record`. See [States](../concepts.md#states-in-hort-ls). |
| `SESSIONS` | Processes running in the sandbox besides its anchor. One shell running one command counts as 2. |
| `AGE` | Time since `hort up` built it. |
| `IDLE` | `active` while anything runs inside; otherwise time since the latest of creation, last attach and last announced completion. |
| `GIT` | How the sandbox was built: `worktree`, or for a [clone-mode](../git-modes.md) sandbox `clone` when the project's repository has every commit in it, `clone, work only in the box` when the clone holds commits it does not, and `clone, work unknown` when hort could not tell. `-` when there is no mode to report: a `lost-record` row, a project without git, or a sandbox whose worktree or clone is gone. |
| `BRANCH` | The sandbox's branch, or `-` without git. |
| `DIRTY` | `dirty` if the worktree has uncommitted changes, `clean` if not. Always `-` for a [clone-mode](../git-modes.md) sandbox, whose `/workdir` is a clone rather than a worktree of your repository. |

A `-` means hort could not tell: there is no record to read (a `lost-record` row), the project has no git (git mode, branch and dirty), the sandbox is in clone mode (dirty), the worktree is gone, or hort could not read the sandbox's process list (sessions, and idle with it). hort shows `-` rather than guessing.

A `lost-record` row is followed by an indented line with the command that collects it.

Age and idle are printed in at most two units, the larger first (`2d 3h`, `41m 3s`), and are rounded down, never up, so an idle time `ls` shows is one [`hort prune --idle`](prune.md) accepts back: a sandbox listed idle `2d 2h` is idle long enough for `hort prune --idle "2d 2h"`. A sandbox younger than a second shows `0s`.

## Behaviour

`ls` never changes anything and never fails because of one sandbox: it compares hort's records, the processes the kernel is running and the worktrees on disk, and reports what it finds. It needs neither `pasta` nor a rootfs and does not read the configuration, so it works even when a configuration file is broken.

`dirty` is checked in each sandbox's own worktree, whatever directory you run `ls` from. A `dirty` sandbox is the one to look at first: it holds work nobody has committed.

A clone's commits are looked for in the repository of the project it was built from, whatever directory you run `ls` from, with the same check [`hort down`](down.md#unreturned-work) asks before removing it. `work only in the box` is the same words `hort prune` skips such a sandbox with: commits that exist nowhere but in that sandbox, which go with it when it is removed. Push them or fetch them out first.

## Messages

`ls` prints nothing but the listing. On an empty machine it prints nothing at all. The general errors in [Commands](index.md#errors-any-command-can-print) can still occur, for example if hort's state directory is unreadable.
