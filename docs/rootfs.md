# Preparing a rootfs

Every sandbox runs on a **prepared rootfs**: an ordinary directory on your host holding a complete Linux root filesystem, with your shell, your tools and your agents already installed. hort never builds, pulls or installs anything. It mounts that directory as the read-only base of every sandbox and gives each sandbox its own disposable writable layer on top, so ten sandboxes share one copy on disk.

You prepare it once, and again whenever you want to change what is inside.

## What a rootfs must provide

1. **A shell at `/bin/sh`.** Sessions drop into a shell, and `hort up` refuses a rootfs without `/bin/sh`.
2. **Tolerance of an arbitrary user.** hort maps your own host user into the sandbox. Do not rely on a fixed `USER` or uid; do not set one.
3. **Your agents, baked in.** Claude Code, Aider, Codex CLI, Gemini CLI: whatever you run must already be installed. hort installs nothing.
4. **A world-writable `/workdir`.** It is where hort binds the worktree. Create it with `mkdir -p /workdir && chmod 1777 /workdir`. hort refuses a rootfs whose `/workdir` is not world-writable.
5. **No command that starts an agent.** hort starts its own idle process as the container's first process. An image `CMD` or `ENTRYPOINT` is ignored.

Beyond those five nothing is checked, and a binary the rootfs does not carry is one no sandbox has. In practice that means:

- `ca-certificates`, for anything that uses TLS.
- `git`, **required** in [clone mode](git-modes.md), where the agent commits and pushes from inside the sandbox. In the default worktree mode no git command works inside (see [Concepts](concepts.md#git-is-a-host-activity)), and the binary is then worth having only for tools that expect it to exist.
- `gh`, if you forward a GitHub token with [the GitHub CLI recipe](recipes.md#github-cli-without-logging-in-each-time), or let an agent open its own pull requests from a clone-mode sandbox.
- The shell you use on the host, if you want it inside (see [the `shell` key](configuration.md#shell)).
- Whatever your own agent hooks call. A Claude Code hook in the `~/.claude` you mount read-only runs inside the sandbox, so a hook built around `jq` needs `jq` here. The completion hook hort installs for [notifications](recipes.md#notifications) needs nothing beyond the shell.

## Agents that refuse to run as root

Inside a sandbox your session is **uid 0 of its own user namespace**. On the host it is your own unprivileged user, and it holds no capabilities, so being "root" in there buys nothing on the machine. Some agents only look at the uid and refuse to run in their unrestricted mode. Claude Code is one:

```text
--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons
```

Claude Code accepts that mode as root when `IS_SANDBOX=1` is set. Set it **in the rootfs**, where the login shell reads it. An image `ENV` line does not survive the export, which keeps files and drops image settings.

- For `sh` and `bash` login shells: `printf 'export IS_SANDBOX=1\n' > /etc/profile.d/hort-sandbox.sh`
- For fish: `mkdir -p /etc/fish/conf.d && printf 'set -gx IS_SANDBOX 1\n' > /etc/fish/conf.d/hort-sandbox.fish`

Other agents have their own switch, or none; check each one's documentation.

## Install into the system, not into a home directory

A sandbox never gets the home directory your build ran as. hort gives every sandbox a home of its own at `/home/hort`, created in memory at boot and holding nothing but the read-only files you chose to mount there, and a session's `PATH` is the plain system one, with no home directory on it (the exact value is in [Configuration](configuration.md#environment-variables)).

An installer that drops its binary under `$HOME` at build time therefore fails in a way that is easy to miss. Nothing is lost: the file is still in the rootfs, under the home the build used, `/root/.local/bin` for example. It is simply under a home no session has, on no `PATH` any session reads, and the line the installer appended to that home's shell configuration is never read either. The build goes green and the tool is missing the first time an agent asks for it.

Install into `/usr/local/bin` instead. `uv` is the usual case, because its installer defaults to `$HOME/.local/bin`:

```dockerfile
RUN curl -LsSf https://astral.sh/uv/install.sh \
    | env UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh \
    && uv --version
```

Ending the step with `uv --version` is deliberate: it runs the binary from the system `PATH` while the build is still going, so a misplaced install fails the build instead of a sandbox weeks later. Any installer with the same habit gets the same treatment.

## Building one from a Dockerfile

A Dockerfile is a convenient way to describe the environment. The repository ships [`example.Dockerfile`](https://github.com/johnsilverio/hort/blob/main/example.Dockerfile): a Node 22 Debian base with `bash`, `git`, `curl`, `ca-certificates`, Claude Code and `IS_SANDBOX=1`. Build it with podman, then **export the container's filesystem into a directory**:

```bash
podman build -t hort-devbox -f example.Dockerfile .
cid=$(podman create hort-devbox)
mkdir -p ~/.local/share/hort/devbox
podman export "$cid" | tar -x -C ~/.local/share/hort/devbox
podman rm "$cid"
chmod 1777 ~/.local/share/hort/devbox/workdir
```

The final `chmod` is not optional: extracting as a normal user drops the mode the image gave `/workdir`.

Then point hort at the directory, in `~/.config/hort/config.json` or a project's `.hort.json`:

```jsonc
{
  "rootfs": "~/.local/share/hort/devbox",
}
```

and check it:

```console
$ hort doctor
...
configuration
  rootfs               ready
```

Use an absolute path or one starting with `~/`. A relative path is read against whatever directory you run hort from.

`debootstrap` or `umoci unpack` produce a rootfs directory just as well. Whatever the tool, the result has to meet the five requirements above.

### A minimal Alpine rootfs

For trying hort out without an agent, a few megabytes of Alpine are enough:

```dockerfile
FROM docker.io/library/alpine:3.22
RUN apk add --no-cache ca-certificates iproute2
RUN mkdir -p /workdir && chmod 1777 /workdir
```

Build and export it exactly as above, into a directory of its own.

## Changing a rootfs

A running sandbox uses its rootfs as the lower layer of a mounted overlay, and changing that layer under a live overlay is undefined. **Never modify a rootfs directory that a live sandbox is using.** Export the new version into a new directory (for example `~/.local/share/hort/devbox-2`), point the configuration at it, and new sandboxes pick it up. Existing sandboxes keep running on the old one until you `hort down` them; then delete the old directory.

Also export into a directory of its own, never into a directory that already holds another rootfs.

## What hort adds on top

You do not need to provide these; hort sets them up in every sandbox:

- A writable home at `/home/hort` and a writable `/tmp`, both in memory and gone when the sandbox goes down, with `HOME` and the `XDG_*` directories pointing into that home.
- Your read-only dotfiles and credentials, placed under `/home/hort` at the same relative path they have under your host home.
- `/etc/resolv.conf` in an open-egress sandbox (and none under an allowlist). It is written into the sandbox's own layer and shadows whatever the rootfs carries.
- The hostname, set to the sandbox name.
- For an agent with `notify.stopHook`, the Claude Code settings file that reports completion.

The rootfs itself is never written to.
