# Commands

| Command | What it does | Changes anything |
| :--- | :--- | :--- |
| [`hort up <name>`](up.md) | Build a sandbox (or finish a half-built one) and open a session in it. | yes |
| [`hort attach <name>`](attach.md) | Open another session in a running sandbox. | records the attach time |
| [`hort run <name> -- <cmd>`](run.md) | Run one command in a running sandbox with no terminal, exiting with its status. | records the attach time |
| [`hort ls`](ls.md) | List every sandbox with its state, sessions, age, idle time, branch and dirty state. | no |
| [`hort down <name>`](down.md) | Tear a sandbox down: processes, container, worktree, record. | yes |
| [`hort prune`](prune.md) | Remove debris and, if asked, idle sandboxes, after confirming. | yes |
| [`hort config`](config.md) | Answer a few questions and write the global configuration. | writes `~/.config/hort/config.json` |
| [`hort doctor`](doctor.md) | Report what this host can do. | no |

`hort help` and `hort <command> --help` print the built-in help. There is no `--version` flag.

## Exit status

- `0`: the command succeeded.
- `1`: hort refused or failed; the reason is printed on stderr as one line.
- For `hort up` without `-d`, `hort attach` and `hort run`, once the session has opened: **the exit status of the session**, or `128 + signal` if it was killed by a signal. A shell you leave with `exit 3` makes hort exit with 3.
- `hort doctor`: `1` when a hard requirement of the host is missing, even though the report is printed.

## Output

Lists and reports (`ls`, `prune`, `doctor`) go to stdout. Errors and `warning:` lines go to stderr. Warnings never stop a command; they tell you something runs degraded.

## Errors any command can print

Messages below that end in a detail come from the system and say what failed.

| Message | Meaning | What to do |
| :--- | :--- | :--- |
| `invalid name` | A sandbox name was empty or contained `/`, or an `egress.allow` entry is not a plain host name. | Pick a name without `/`; write allowlist entries as `example.com` or `*.example.com`. |
| `state directory error: ...` | hort could not read or write its own files under `~/.local/state/hort` or determine your home directory. | Check permissions and free space on that path. |
| `working directory failed: ...` | The directory you ran hort from could not be read (for example it was deleted). | `cd` somewhere that exists. |
| `corrupt metadata: ...` | A sandbox record on disk cannot be read. | `hort prune` offers to remove corrupt records. |
| `git command failed: <operation>: <git's message>` | A git operation hort ran failed; git's own message follows. | Read git's message; run the same operation yourself in the repository to see more. |
| `container runtime failed: ...` | Building, joining or removing a container failed. | See [Troubleshooting](../troubleshooting.md). |
| `sandbox networking failed: ...` | Starting or stopping pasta, the proxy or a database forwarder failed. | See [Troubleshooting](../troubleshooting.md). |
