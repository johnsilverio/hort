# Working inside a hort sandbox

*This page is written for a coding agent. Paste it into the agent's instructions (for example a project's `CLAUDE.md` or `AGENTS.md`), or point the agent at it.*

---

You are running inside a **hort sandbox**: an isolated Linux container around a git worktree. It exists so you can work with full permissions without any risk to the user's machine. These facts hold for every hort sandbox:

## Where you are

- Your working directory is **`/workdir`**. It is the user's repository, on a branch created for this task, either as a worktree or as a clone (see [Git](#git) below for how to tell and what changes). Everything you should change is in there.
- `HORT_SANDBOX` holds the sandbox name, which is also the branch name and the hostname.
- `HORT_WORKTREE` holds the **host** path of `/workdir`. It does not exist inside the sandbox; it is useful only when telling the user where to look.
- Your home is `/home/hort`. It lives in memory.
- The user's real home directory and the rest of the host filesystem do not exist here. Some of the user's configuration files may be mounted read-only under `/home/hort`.

## What you can and cannot do

- You are uid 0, but only inside your own user namespace, with no capabilities. `sudo` gains nothing; you cannot mount filesystems, change the network or load kernel modules.
- You can write anywhere in the filesystem: `/workdir`, your home, `/tmp`, even `/usr` and `/etc`. Whether the system package manager works depends on the image; anything it installs is lost when the sandbox goes away.
- **Only two places persist** after the sandbox is torn down: files in `/workdir` that the user commits, and the dependency cache directories the project declares (typically `/workdir/node_modules` or directories under `/home/hort/.cache`). Everything else, including your home and anything installed outside those, is discarded.
- Configuration files mounted read-only (dotfiles, credentials) cannot be modified; writes to them fail with "Read-only file system". Do not try to work around it.

## Git

**First, find out which mode you are in.** Run `git -C /workdir status`. It either works or it does not, and that decides everything below. Do not assume; check.

### If git does not work (the default mode)

- The worktree's `.git` file points to the user's repository on the host, which does not exist here, so `git status`, `git diff`, `git commit` and every other git command fail. This is intentional: it keeps the real repository's history and branches out of your reach.
- **Do not commit, and do not try to repair git** (do not run `git init`, do not delete or rewrite `/workdir/.git`, do not clone the repository again). Destroying `.git` in `/workdir` only breaks the user's review of your work.
- Write your changes as files. The user reviews them with `git diff` on the host and commits them there.
- When you finish, summarize what you changed and which files, since you cannot show a diff yourself.
- **The GitHub CLI (`gh`) may still work for API operations** if the user forwarded a token: you can open a pull request, read issues or call `gh api` against a branch that already exists on the remote. You cannot commit or push from here, because that needs git. If `gh` reports it is not authenticated, tell the user; do not run `gh auth login`.

### If git works (clone mode)

- `/workdir` is a **clone of the user's repository**, made for this sandbox, already on a branch named after it. Commit there as you normally would.
- `origin` is the user's own remote, and that is where your work goes. Push your branch and open a pull request with it.
- `hort-base` points back at the user's repository on this machine. It is **fetch only** and a push through it fails by design. Do not try to work around that.
- `/run/hort/objects` holds the history your clone borrows, mounted read-only. Never try to write there, and do not run `git gc`, `git repack` or anything else meant to rewrite the object store you did not create.
- If a push fails because you have no credentials, say so and stop. Do not run `gh auth login`, do not invent a remote, and never write a token into the repository.
- Rewriting history you did not create (a force push, a rebase of the base branch) reaches the user's remote. Do not do it unless the task explicitly asks for it.

## Network

The project decides one of two modes.

**Open (the default).** You can reach the internet directly, and DNS works. If `HTTP_PROXY` is **not** set, you are in this mode.

**Allowlist.** If `HTTPS_PROXY` is set (to `http://127.0.0.1:<port>`), the sandbox can only reach hosts the user allowed, through that proxy.

- Use `https://` URLs with tools that honour `HTTPS_PROXY` (curl, pip, npm and most language HTTP clients do).
- Plain `http://` requests through the proxy are refused. DNS lookups fail; the proxy resolves names. Direct connections to IP addresses fail.
- A connection refused by the proxy (HTTP 403) means the host is not on the allowlist. Do not try to bypass it; tell the user which host you needed, so they can decide whether to add it.

## Databases and local services

Databases the project declares are reachable at **`127.0.0.1:<port>`**, for example PostgreSQL at `127.0.0.1:5432`, in both network modes. `NO_PROXY` already covers `127.0.0.1` and `localhost`. Use only the development credentials the project provides. There is no production data here, and you must not look for any.

## Processes and sessions

- The sandbox's first process (`sleep infinity`, PID 1) keeps it alive. You cannot stop it, and you must not try.
- The user may have several shells and other agents running in this sandbox at the same time, working on the same files. Do not kill processes you did not start. Other sessions' shells are processes you did not start.
- Exiting your own shell ends only your session. The sandbox keeps running until the user tears it down from the host.

## If something fails

- "Read-only file system" on a file under `/home/hort`: it is the user's mounted configuration; leave it alone.
- "Network unreachable", DNS failures, or HTTP 403 from the proxy under an allowlist: report the host you needed.
- git errors in the default mode: expected; see above. In clone mode git works, so an error there is real and worth reporting.
- A tool the task needs is not installed and cannot be installed: say so; the user adds it to the sandbox image.
