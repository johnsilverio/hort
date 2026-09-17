# hort ls

List every sandbox on this machine with its state.

```text
hort ls

Options:
  -h, --help  Print help
```

## Output

One line per sandbox, from every project, with no header:

```console
$ hort ls
fix-login  live  2  41m 3s 211ms 802us 45ns  active  fix-login  dirty
notes  live  0  35s 73ms 521us 360ns  15s 535ms 72us 847ns  -  -
old-spike  orphaned  0  2days 3h 4m 10s 5ms 1us 7ns  2days 2h 58m 1s 3ms 2us 9ns  old-spike  clean
ghost  lost-record  0  -  -  -  -
    running with no record on disk; run 'hort down ghost' to stop its container and host-side helpers
```

The columns, separated by two spaces:

| # | Column | Meaning |
| :--- | :--- | :--- |
| 1 | name | The sandbox name. |
| 2 | state | `live`, `orphaned`, `inconsistent` or `lost-record`. See [States](../concepts.md#states-in-hort-ls). |
| 3 | sessions | Processes running in the sandbox besides its anchor. One shell running one command counts as 2. |
| 4 | age | Time since `hort up` built it. |
| 5 | idle | `active` while anything runs inside; otherwise time since the latest of creation, last attach and last announced completion. |
| 6 | branch | The sandbox's branch, or `-` without git. |
| 7 | dirty | `dirty` if the worktree has uncommitted changes, `clean` if not. |

A `-` means hort could not tell: there is no record to read (a `lost-record` row), the project has no git (branch and dirty), the worktree is gone, or hort could not read the sandbox's process list (sessions, and idle with it). hort shows `-` rather than guessing.

A `lost-record` row is followed by an indented line with the command that collects it.

Durations are printed at full precision, down to nanoseconds.

## Behaviour

`ls` never changes anything and never fails because of one sandbox: it compares hort's records, the processes the kernel is running and the worktrees on disk, and reports what it finds. It needs neither `pasta` nor a rootfs and does not read the configuration, so it works even when a configuration file is broken.

`dirty` is checked in each sandbox's own worktree, whatever directory you run `ls` from. A `dirty` sandbox is the one to look at first: it holds work nobody has committed.

## Messages

`ls` prints nothing but the listing. On an empty machine it prints nothing at all. The general errors in [Commands](index.md#errors-any-command-can-print) can still occur, for example if hort's state directory is unreadable.
