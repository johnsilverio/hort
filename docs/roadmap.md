# Roadmap

This page lists work that has been decided but is **not available yet**. It describes what you will be able to do, not when: there are no dates and no promised order, and any of it may change. Everything else in this book describes hort as it works today.

## Clone mode, opt-in

Today git does not work inside a sandbox, and you commit from the host ([why](concepts.md#git-is-a-host-activity)). A planned opt-in mode gives a sandbox its own clone of the repository, so an agent can commit, create branches and open a pull request with its own tools. Your host repository is never written from inside. Pushing to a remote such as GitHub uses a narrowly scoped token you choose to pass in, so what the agent can do there is bounded by that token and by the remote's branch protection.

On large repositories a clone costs noticeably more disk space and time than a worktree, which is why this mode will be opt-in and worktrees stay the default.

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
