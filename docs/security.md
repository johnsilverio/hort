# Security model

A security tool that oversells itself is worse than none. This page says exactly what hort protects, how, and where it stops.

**In one sentence:** hort contains destruction, and it mitigates exfiltration when you configure an allowlist, but it does not eliminate it. Run hort on repositories you trust, with development credentials only, never production.

## What hort protects

### Your filesystem

A sandbox runs in its own mount namespace, pivoted into its own root. **The host filesystem does not exist inside**: `cd ..` from `/workdir` reaches the sandbox's `/`, and there is no path that leads to your home directory, your SSH keys or other projects. This is enforced by the kernel's namespaces, not by wrappers or aliases.

What the sandbox can see and write:

| Surface | Inside | Writable | Survives `hort down` |
| :--- | :--- | :--- | :--- |
| The worktree, or the clone | `/workdir` | yes | the directory is deleted; in worktree mode committed work survives on the branch, and in clone mode only what left the sandbox, by a push or a fetch from the host |
| Declared caches | `/workdir/<name>` or their `target` | yes | yes, on the host under hort's state, shared by the project's sandboxes |
| Everything else in `/` | the rootfs plus a per-sandbox layer | yes | no, the layer is discarded |
| `HOME` (`/home/hort`) and `/tmp` | in memory | yes | no |
| Dotfiles and credentials you configure | under `/home/hort` | **no**, read-only | not applicable |
| The rootfs itself | the base of `/` | never modified | not applicable |

The whole root is writable, so tools that write to `/usr` or `/etc` work. The guarantee is not "only these paths are writable". It is: **no write persists past `down` except in the worktree and the caches, no write reaches the host or another sandbox, and the base is never modified.**

### Your repository

**In worktree mode, the default.** The real `.git` directory stays on the host and is not mounted. The agent sees a worktree whose `.git` is a pointer file naming a host path that does not exist inside. That pointer is mounted **read-only**: the agent cannot rewrite or replace it, and the mount point itself cannot be removed or renamed from inside. It can delete every other file in `/workdir`; your history, your other branches and your main checkout are untouched. **The most a rogue command can destroy is the uncommitted content of one worktree.** That is also why git does not work inside a sandbox in this mode.

You commit the work from the host, in that same worktree, while the sandbox is up. The read-only pointer is what makes that safe: a `git` you run there reads the genuine pointer, not one the agent rewrote to name a repository it planted with its own hooks or filters. Such a planted pointer would run the agent's configuration as you, on the host, outside every layer hort has, the moment you ran an ordinary git command in the worktree. Committing advances the sandbox's own branch in your repository; the commits are yours, made on the host.

#### In clone mode

[Clone mode](git-modes.md) lets the agent run git itself, and the guarantee about your repository is unchanged: it is never written from inside. `/workdir` is a clone with its own writable `.git`, and your repository reaches it in two ways only, both of them one-way:

- its **object store is mounted read-only**, so the clone reads all of your history and can rewrite none of it (a write there is refused by the kernel with `Read-only file system`);
- the remote pointing back at your repository, `hort-base`, is **fetch only**, because a plain shared clone would otherwise be able to push new branches straight into it.

After an agent commits inside such a sandbox, your repository's `.git` is byte for byte what it was. The one thing hort writes there is a ref, `refs/hort/<name>/base`, which pins the commit the clone started from so your own `git gc` cannot prune objects the clone borrows. `hort down` deletes that sandbox's ref along with the sandbox, leaving the refs of your other clone-mode sandboxes alone.

What clone mode does change is where a credential lives. If you want the agent to push, you forward a token into a box you are not watching, so the token is what bounds the damage: scope it to one repository, give it only the permissions the job needs, and protect the branches on the remote.

### Your machine

- **No capabilities.** Every process in the sandbox runs with an empty capability set. `sudo` inside has nothing to grant.
- **A user namespace.** Inside, a session is uid 0 of its own user namespace. On the host it is your own unprivileged user. Files written to `/workdir` belong to you on the host.
- **A syscall filter.** Every process runs under the default seccomp profile container runtimes ship.
- **Landlock** adds filesystem and, under an allowlist, network restrictions where the kernel supports them.
- **A resource ceiling**, when you configure `resources` and your user has the cgroup controllers delegated, bounds memory and CPU so a runaway loop cannot starve the host.
- **No root and no daemon.** hort runs entirely as your user. There is no setuid helper and no privileged service to attack.

## What hort does not protect

### Exfiltration under open egress

By default a sandbox has open network access, because agents need to reach their model provider. A hostile repository, or an agent tricked by a prompt injection, can send anything it can read to any server. That includes services listening on your host's loopback interface, which are reachable at `127.0.0.1` from inside an open sandbox.

### What you mount into the box

Read-only means the agent cannot modify your dotfiles and credentials. It can still **read** them, and under open egress it can send them anywhere. Mount only what the agent needs, and only development or personal session credentials. Environment variables you forward with `auth.env` are equally readable.

### Abuse of an allowed host

An egress allowlist closes arbitrary destinations, but a host you allow can still carry data out. If `github.com` is on the list, a malicious push to a repository the agent controls is still egress.

### The allowlist's dependence on SNI

The proxy confirms each connection's destination by reading the host name in the TLS handshake, without decrypting anything. If Encrypted Client Hello becomes common, that confirmation stops working; the allowlist of host names requested from the proxy still applies, but a client could name an allowed host and reach a different one sharing the same infrastructure.

### Malicious repositories

hort is not armor against code designed to escape containers or exploit the kernel. Its layers raise the cost of that considerably; they are not a guarantee.

## How the allowlist is enforced

When `egress` is an allowlist, four layers stack:

1. **A network namespace the sandbox does not own, with no routes.** hort's own user namespace owns it, so the agent, root in a different user namespace, has no network privileges over it and cannot add a route. The only addresses reachable are the proxy and the declared databases, spliced onto the sandbox's loopback.
2. **Landlock** (Linux 6.7+) restricts which ports a session may connect to, irreversibly.
3. **The proxy** tunnels only `CONNECT` requests to allowed hosts, and only when the TLS handshake names the same host. It logs every decision.
4. **No DNS.** The sandbox has no resolver; the proxy resolves names on the host, so DNS cannot be used as a side channel.

Details, and how to read the proxy log, are in [Networking and egress](networking.md).

## What an agent inside can and cannot end

Nothing inside a sandbox can end the sandbox. Its first process is an idle anchor, and the kernel discards signals sent to that process from inside its own namespace, so `kill -9 1` does nothing. Only `hort down`, run on the host, tears a sandbox down.

What an agent **can** end is sessions. Every session runs as the same user in the same process namespace, so a process in one session can kill processes in another, including the shell you are typing in. An `exit` an agent runs in its own tool shell ends only that shell, but an agent that runs commands directly in the session's shell can end that session.

When that happens, nothing is lost: the sandbox, its files and every other session keep running. Open a new session with `hort attach <name>`.

## The rules that follow

- Run hort on repositories you trust.
- Mount and forward development or personal credentials only. Never production.
- Point databases at development data only.
- Turn on an egress allowlist when you leave an agent working unattended on anything you are less than sure about.
- Commit work you want to keep before `hort down`.
