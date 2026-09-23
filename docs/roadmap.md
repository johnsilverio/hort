# Roadmap

This page lists work that has been decided but is **not available yet**. It describes what you will be able to do, not when: there are no dates and no promised order, and any of it may change. Everything else in this book describes hort as it works today.

## Closing the host's loopback to an open sandbox

Today an open sandbox reaches every service listening on your host's `127.0.0.1`: a development database, a cache with no password, an admin panel a project left running. That is the default mapping of the network helper, and hort unmaps it only under an [allowlist](networking.md#an-egress-allowlist), so the only way to close it is to change the whole egress posture. Planned: an open sandbox reaches the host's loopback only at the ports its [declared databases](networking.md#databases) name, and the rest of that interface is closed unless you ask for it.

## Preparing a rootfs with hort itself

Today you build a rootfs, export it into a directory and fix the mode of its `/workdir` by hand ([Preparing a rootfs](rootfs.md#building-one-from-a-dockerfile)), and a forgotten final `chmod` makes `hort up` refuse the directory after all that work. Planned: `hort rootfs import` takes the tarball `podman export` or `docker export` writes, unpacks it into a directory of its own, sets the mode `/workdir` needs, checks it against [what a rootfs must provide](rootfs.md#what-a-rootfs-must-provide) and records the path in your configuration.

## A credentials question that says what it exposes

Today the agent question in `hort config` offers to mount an agent's credential directory read-only and names nothing but the directory ([the dialogue](commands/config.md#the-dialogue)). It has no default answer, so nothing is mounted until you press `y`. Read-only keeps the agent from changing those credentials; it does not keep it from reading them and, under open egress, sending them on ([what you mount into the box](security.md#what-you-mount-into-the-box)). Planned: the question says what mounting the directory exposes before you answer it.

## Saying what posture a sandbox starts in

Today nothing at `up` time tells you which posture the box you just opened runs in. Open egress and mounted credentials are each documented, and the rule against leaving them together on an unattended box is written down ([the rules that follow](security.md#the-rules-that-follow)), but you have to remember what you configured to know that is what you have. Planned: `hort up` names the posture it built, open or allowlisted, and what it mounted, so the two are in front of you before you walk away from the box.

## Installing without a Rust toolchain

Today hort is built from source, which means a Rust toolchain, the libseccomp headers and a release build before the first sandbox ([Building the binary](installation.md#building-the-binary)). Planned: a released binary for Linux you download and run, so trying hort does not begin with a compiler.

## Cleaning up after clone mode

[Clone mode](git-modes.md) works today. One piece of it is still missing:

- `hort ls` does not show which git mode a sandbox was built in, and its dirty column reads `-` for a clone. Planned: the mode is visible in the listing.

## Deleting the sandbox's branch on `down` and `prune`

Today `hort down` keeps the sandbox's branch and you delete it by hand with `git branch -d`. A planned `--delete-branch` flag (`-b`) on `hort down` and `hort prune` removes it too:

- a branch fully merged is deleted;
- a branch with unmerged commits is kept unless you confirm on a terminal (the default answer is no); without a terminal it is kept, and hort prints the command to delete it by hand.

## Colors inside sessions

Today a session does not receive `TERM` or `COLORTERM`, so programs such as Claude Code start without colors, and you set them in your shell configuration ([workaround](recipes.md#colors-inside-the-sandbox)). Planned: a session opened on a terminal receives your host's `TERM` and `COLORTERM`.

## Friendlier `hort config`

Planned: the rootfs question suggests, as its default, a prepared rootfs directory hort finds on your machine, and the dotfile list says how to select more than one item.

## Worktree cleanup from outside the repository

Planned: when `hort down` or `hort prune` runs from outside the sandbox's repository, the repository's record of the removed worktree is cleaned up correctly instead of being left stale. Today, running `git worktree prune` in that repository clears it.

## Adopting a sandbox whose record was lost

Today a `lost-record` sandbox can only be stopped with `hort down` ([details](troubleshooting.md#a-lost-record-sandbox)). Planned: adopting such a sandbox back under hort's management, when every piece of its record can be recovered from the system.
