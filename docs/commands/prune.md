# hort prune

Clean up: sandboxes whose container is gone, broken records, caches of deleted projects, and, if you ask, sandboxes that have been idle a long time. It always shows what it will remove and asks first.

```text
hort prune [OPTIONS]

Options:
  -f, --force        Skip the confirmation prompt and the dirty exclusion
      --idle <IDLE>  Also remove sandboxes idle at least this long
  -h, --help         Print help
```

## Examples

```bash
hort prune               # debris only
hort prune --idle 7d     # debris, plus live sandboxes idle for a week or more
hort prune --idle 36h -f # no question, and dirty worktrees too
```

`--idle` takes a duration such as `90m`, `24h` or `7d`.

`prune` is global: it considers every sandbox and cache on the machine, whatever project you run it from.

## What it considers

**Sandboxes:**

- `orphaned` and `inconsistent` sandboxes are always candidates.
- `live` sandboxes are candidates only with `--idle`, and only when idle at least that long. A sandbox with anything running inside is `active`, never idle, so `prune` never removes a sandbox someone is working in.
- `lost-record` sandboxes are never touched by `prune`. Use `hort down <name>`.

**Broken records:** sandbox directories whose record cannot be read.

**Caches:** the dependency caches of projects.

## What protects a candidate

Without `-f`, a candidate is removed only when hort can **prove** nothing is at risk:

- a sandbox whose worktree has uncommitted changes is skipped as `dirty`;
- a sandbox whose worktree state cannot be read is skipped as `unknown`;
- a cache whose project directory still exists is skipped as `project on disk`;
- a cache whose project hort cannot check is skipped as `project unreadable`.

`-f` removes those too. What `-f` never overrides:

- a cache that a running sandbox is using is skipped as `sandbox running`: `hort down` that sandbox first;
- if hort finds a running container it cannot attribute to a project (for example a `lost-record` sandbox), every cache is skipped as `sandbox unaccounted for`, because that container might be using it;
- with `--idle`, a live sandbox whose idle time could not be read is skipped as `idle unknown`.

## Confirmation

With something to remove and no `-f`, prune lists it and asks:

```text
prune old-spike, /home/you/src/deleted-project? this removes their worktrees, metadata and caches [y/N]
```

Only `y` or `yes` proceeds. Without a terminal and without `-f` it refuses:

```text
refusing to prune without confirmation: stdin is not a TTY (pass --force to proceed)
```

When there is nothing to remove it does not ask.

## Output

One line per result:

```text
removed old-spike
removed cache of /home/you/src/deleted-project
skipped experiment (dirty)
skipped /home/you/src/webapp (project on disk)
```

Every sandbox it removes is torn down in the same order as [`hort down`](down.md): helpers, container, worktree, record. Caches are removed last. Finally, prune runs `git worktree prune` in the repository you ran it from, clearing registrations of worktrees that no longer exist there (correct cleanup when run from outside the sandbox's repository is [planned](../roadmap.md#worktree-cleanup-from-outside-the-repository)). **Run `hort prune` from inside a git repository**: that last step needs one, and from anywhere else it ends with a `git command failed: worktree prune: fatal: not a git repository ...` error.

## Messages

| Message | Meaning and what to do |
| :--- | :--- |
| `refusing to prune without confirmation: stdin is not a TTY (pass --force to proceed)` | There is something to remove and nobody to ask. |
| `git command failed: ...` | A worktree removal or the final `git worktree prune` failed; git's message says why. |
| `container runtime failed: ...` / `sandbox networking failed: ...` / `state directory error: ...` | Removing one sandbox or cache failed. Run `hort ls` to see what is left and retry. |
