# Running agents in parallel

hort gives you two ways to run agents side by side, and tmux (or any terminal multiplexer, or just several terminal tabs) to arrange them. hort itself never holds your terminal open for you, never detaches and reattaches a session, and has no layout of its own: that is the multiplexer's job, and it already does it well.

## Two shapes of parallel

**Several agents in one sandbox.** Every `hort attach` opens another session in the same box, on the same worktree and branch. Use it when the agents are working on the same change: one writing a feature, one reviewing the diff as it grows, one running the tests in a loop.

**One sandbox per task.** Each `hort up <name>` gets its own branch, its own worktree and its own container. Agents in different sandboxes cannot see each other's files at all. Use it for independent tasks you want to review and merge separately.

You can mix them: three sandboxes, two sessions in one of them.

Conflicts are yours to manage. Two agents editing the same file in one sandbox can overwrite each other, exactly as two processes in one directory would on your machine. Two sandboxes never conflict on disk, but their branches can conflict when you merge.

## With tmux

Build the sandboxes detached, then give each session its own pane:

```bash
cd ~/src/webapp
hort up fix-login -d
hort up search-api -d

tmux new-session -d -s webapp 'hort attach fix-login'
tmux split-window -h -t webapp 'hort attach fix-login'
tmux new-window -t webapp 'hort attach search-api'
tmux attach -t webapp
```

Now start an agent in each pane, for example `claude --dangerously-skip-permissions` in the first, `aider` in the second.

A pane whose session ends simply closes. Neither closing a pane nor killing the whole tmux server touches the sandboxes:

```console
$ tmux kill-server
$ hort ls
NAME        STATE  SESSIONS  AGE     IDLE    GIT       BRANCH      DIRTY
search-api  live   0         1h 11m  1h 11m  worktree  search-api  clean
fix-login   live   0         1h 12m  3s      worktree  fix-login   dirty
```

Reattach whenever you like, with a new pane and `hort attach fix-login`. The same holds over SSH: sandboxes on a remote machine keep running when the connection drops. Run tmux on the remote machine and your panes survive too.

## Knowing which box a shell is in

Every session has two environment variables:

- `HORT_SANDBOX`: the sandbox name, for example `fix-login`.
- `HORT_WORKTREE`: the host path of the directory mounted at `/workdir`.

hort does not change your prompt. If your dotfiles are mounted into the sandbox, add a segment that shows `HORT_SANDBOX` when it is set. For fish:

```fish
# in config.fish or a function called from fish_prompt
if set -q HORT_SANDBOX
    echo -n "[$HORT_SANDBOX] "
end
```

For bash:

```bash
[ -n "$HORT_SANDBOX" ] && PS1="[$HORT_SANDBOX] $PS1"
```

Inside a sandbox the hostname is also the sandbox name, so prompts that show `\h` already say where you are.

## Keeping track

`hort ls` lists every sandbox on the machine, from every project, with its sessions, idle time and whether its worktree holds uncommitted work. A sandbox you forgot keeps running until you remove it; it costs memory, never work. Review and commit from the host, then `hort down` what you are finished with.

## Notifications

If an agent can announce that it finished, hort can raise a desktop notification on the host when it does, so you do not have to watch every pane. Claude Code can; see [Notifications](recipes.md#notifications).
