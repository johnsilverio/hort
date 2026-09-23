# Installation

hort is a single binary written in Rust. You build it from source, put it on your `PATH`, and check that your host can run sandboxes with `hort doctor`.

## Host requirements

hort runs on **Linux only**, as your normal unprivileged user. It needs no root and no daemon.

| Requirement | Needed for | If it is missing |
| :--- | :--- | :--- |
| Unprivileged user namespaces | Every sandbox | No sandbox can be built at all. |
| [`pasta`](https://passt.top/) on `PATH` (the `passt` package) | Sandbox networking | `hort up` refuses to start. |
| `git` on `PATH` | Every `hort up`, repository or not | `hort up` refuses to start. |
| Overlay filesystem usable inside a user namespace | Every sandbox root | A build fails when it mounts the root. |
| `libseccomp` (the shared library) | Running the `hort` binary | The binary does not start. |
| `ip` on `PATH` (the `iproute2` package) | Only sandboxes with an egress allowlist | `hort up` refuses an allowlisted sandbox. |
| cgroup v2 `memory` and `cpu` controllers delegated to your user | Only a `resources` ceiling | The ceiling is dropped with a warning; the sandbox still starts. |
| Landlock ABI 4 or later (Linux 6.7+) | One of the layers of an egress allowlist | The allowlist runs without that layer, with a warning. |
| `notify-send` on `PATH` (libnotify) | Only desktop notifications | No notification is raised, with a warning. |

You also need a **prepared rootfs directory**: the base filesystem, with your agents inside, that every sandbox runs. That has its own page, [Preparing a rootfs](rootfs.md).

## Building the binary

You need a Rust toolchain, version 1.89 or newer, and the libseccomp development package (`libseccomp-devel` on Fedora, `libseccomp-dev` on Debian and Ubuntu). A released binary, so that none of this is needed to try hort, is [planned](roadmap.md#installing-without-a-rust-toolchain).

```bash
git clone https://github.com/johnsilverio/hort.git
cd hort
cargo build --release --locked
mkdir -p ~/.local/bin
install -m 0755 target/release/hort ~/.local/bin/hort
```

Make sure `~/.local/bin` is on your `PATH`, then:

```console
$ hort --help
The parsed command line: one subcommand and its flags

Usage: hort <COMMAND>

Commands:
  up      Build a sandbox and open a session in it
  attach  Open one more session in a running sandbox
  run     Run one command in a running sandbox with no terminal
  ls      List every sandbox with its reconciled state
  down    Tear a sandbox down in the mandatory order
  prune   Remove idle sandboxes and abrupt-death debris after confirming
  config  Ask what this host can do and write the global configuration
  doctor  Report what this host can do, changing nothing
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

The binary links dynamically against the C library and `libseccomp`, so copy it only to machines that have both.

## Checking your host with `hort doctor`

`hort doctor` reads the host and prints one row per capability, changing nothing. Run it now, and again whenever you change something on the host.

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
  rootfs               no rootfs configured — set "rootfs" to a prepared rootfs directory in .hort.json or ~/.config/hort/config.json
```

Under each thing the host lacks, doctor says what that costs and how to get it. The last row reports your configured rootfs with the same message `hort up` would print, or `ready` once it can carry a sandbox.

The exit status is `0` when the three hard requirements (user namespaces, `pasta`, `git`) are met and `1` otherwise, whatever the rootfs row says, so you can use it as a gate in a script:

```bash
hort doctor > /dev/null && echo "this host can build sandboxes"
```

See [hort doctor](commands/doctor.md) for every row.

## Fixing what doctor reports

**User namespaces.** Most distributions allow unprivileged user namespaces by default. If doctor says `no`, check the `user.max_user_namespaces` sysctl (it must be above zero). On Ubuntu 23.10 and later, AppArmor restricts them through the `kernel.apparmor_restrict_unprivileged_userns` sysctl; read your distribution's guidance before relaxing it.

**pasta, git, ip, notify-send.** Install the `passt`, `git`, `iproute2` and `libnotify` packages of your distribution.

**cgroup controllers.** systemd delegates only `memory` and `pids` to users by default. A `resources.cpus` ceiling needs `cpu` as well. Add a drop-in and restart your user session:

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload
```

Then log out completely and back in (or reboot) and run `hort doctor` again. hort caps CPU time, never pins cores, so `cpuset` is reported but not used by any setting.

**Landlock.** The Landlock row matters only for egress allowlists. Below ABI 4 an allowlisted sandbox loses its kernel port restriction; the other layers still hold. See [Networking and egress](networking.md).

## Where hort keeps things

| What | Where |
| :--- | :--- |
| Global configuration | `~/.config/hort/config.json` (or `$XDG_CONFIG_HOME/hort/config.json`) |
| Per-sandbox records, overlays, worktrees, notify channels | `~/.local/state/hort/sandboxes/<name>/` (or under `$XDG_STATE_HOME/hort`) |
| Dependency caches | `~/.local/state/hort/cache/<project>/` |
| Container state, helper pid files and the per-sandbox log | `$XDG_RUNTIME_DIR/hort/` (usually `/run/user/<uid>/hort/`), emptied on reboot |

Worktrees live under hort's state directory, not next to your repository, so nothing is added to your project folder.

## Next

[Prepare a rootfs](rootfs.md), then follow the [Quickstart](quickstart.md).
