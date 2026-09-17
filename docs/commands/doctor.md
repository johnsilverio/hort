# hort doctor

Report what this host can do, changing nothing.

```text
hort doctor

Options:
  -h, --help  Print help
```

## Output

```console
$ hort doctor
host
  user namespaces      yes
  pasta                /usr/sbin/pasta
  ip                   /usr/sbin/ip
  cgroup memory        delegated
  cgroup pids          delegated
  cgroup cpu           not delegated
    a sandbox runs with no CPU ceiling. Add cpu to Delegate= in a systemd drop-in for user@.service.
  cgroup cpuset        not delegated
    a sandbox cannot be pinned to a set of cores. Add cpuset to Delegate= in a systemd drop-in for user@.service.
  landlock             ABI 7
  rootless overlayfs   yes
  notify-send          /usr/sbin/notify-send
  git                  yes

configuration
  rootfs               ready
```

Every row is read from the live host. Programs are reported with the path where they were found on your `PATH`. Under each missing capability, an indented line says what that costs and how to get it.

| Row | Needed for | When missing |
| :--- | :--- | :--- |
| user namespaces | everything | `a sandbox is a user namespace, so nothing gets built here at all. Look at the user.max_user_namespaces sysctl, and at whatever security profile your distribution ships.` |
| pasta | everything | `no sandbox gets built: up refuses before it starts, because pasta is what bridges one to the network. It ships in the passt package.` |
| ip | egress allowlists | `only an egress allowlist needs it, to empty the sandbox's routing table; an open sandbox builds fine without it. It ships in iproute2.` |
| cgroup memory | `resources.memory` | `a sandbox runs with no memory ceiling. Add memory to Delegate= in a systemd drop-in for user@.service.` |
| cgroup pids | nothing configurable | `nothing caps how many processes a sandbox forks. Add pids to Delegate= in a systemd drop-in for user@.service.` |
| cgroup cpu | `resources.cpus` | `a sandbox runs with no CPU ceiling. Add cpu to Delegate= in a systemd drop-in for user@.service.` |
| cgroup cpuset | nothing configurable | `a sandbox cannot be pinned to a set of cores. Add cpuset to Delegate= in a systemd drop-in for user@.service.` |
| landlock | one layer of egress allowlists | `unavailable`, then: `an allowlisted sandbox loses the kernel restriction on which ports a session may dial; the routeless namespace, the proxy and the absent resolver still hold. ABI 4 or later carries that port half.` |
| rootless overlayfs | everything | `every sandbox root is an overlay, so a build gets as far as the mount and dies there. The kernel has to offer overlay to an unprivileged user namespace.` |
| notify-send | desktop notifications | `an agent announcing that it finished gets recorded and nothing reaches the screen. It ships in libnotify.` |
| git | everything | `no sandbox gets built, in a repository or in a marked folder alike: up refuses before it starts, because git is what tells those two apart and what prepares the worktree. Install it.` |

How to fix each is in [Installation](../installation.md#fixing-what-doctor-reports).

## The configuration row

doctor reads the configuration that `hort up` would read from the directory you run it in.

- `rootfs  ready`: the configured rootfs can carry a sandbox.
- `rootfs  <message>`: the exact message `hort up` would stop with, for example `no rootfs configured — ...` or `rootfs '<path>': /workdir is not writable by the mapped uid — make it world-writable`.
- `config file  invalid config: <file>: <parser message>`, followed by `everything above is still what this host can do; nothing here speaks for this project until that file parses.`: a configuration file does not parse. The host rows are still printed.

Warnings from reading the configuration (for example about a devcontainer file) are printed on stderr after the report.

## Exit status

`0` when user namespaces, `pasta` and `git` are all present; `1` otherwise. The rootfs and the optional capabilities do not affect it, so a freshly set up machine with no rootfs yet still passes. Use it as a gate:

```bash
if hort doctor > /dev/null; then
    hort up fix-login -d
fi
```
