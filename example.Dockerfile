# example.Dockerfile: a starting point for a hort base rootfs.
#
# hort does NOT run this image. It runs a prepared rootfs *directory*. This Dockerfile is a
# convenient way to *describe* the environment; you build it and then EXPORT it to a directory,
# which hort mounts as the read-only base of every sandbox:
#
#   podman build -t hort-devbox -f example.Dockerfile .
#   cid=$(podman create hort-devbox)
#   mkdir -p ~/.local/share/hort/devbox
#   podman export "$cid" | tar -x -C ~/.local/share/hort/devbox
#   podman rm "$cid"
#   chmod 1777 ~/.local/share/hort/devbox/workdir
#   # then point "rootfs" in .hort.json or ~/.config/hort/config.json at that directory.
#
# Export into a directory of its own, never into one a sandbox already uses as its rootfs.
# The last chmod is not optional: extracting as a normal user drops the mode this image gives
# /workdir, and hort refuses a rootfs whose /workdir is not world-writable.
#
# (debootstrap or `umoci unpack` are equally valid ways to materialize the directory.)
#
# The contract this rootfs must honor:
#   1. Provide a shell. The first session and every `hort attach` drop into it.
#   2. Tolerate an ARBITRARY uid. hort runs rootless and maps the in-container user to the
#      host uid that owns the worktree, so NOTHING may hardcode a USER or assume a fixed uid.
#   3. Bake in the agent binaries you run. hort never installs agents.
#   4. Carry a world-writable /workdir, where hort binds the worktree.
#   5. Do NOT define a CMD/ENTRYPOINT that runs an agent. hort injects `sleep infinity` as
#      the container's init (the anchor). A CMD here would be ignored at best, harmful at worst.

# Claude Code needs Node 22 or newer, and Debian's own nodejs package is older than that,
# so start from the official Node image. Check `npm view @anthropic-ai/claude-code engines`
# when you swap agents or bump versions.
FROM docker.io/library/node:22-trixie-slim

# A shell and the basics agents and dotfiles expect. ca-certificates is needed for TLS egress.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        less \
    && rm -rf /var/lib/apt/lists/*

# Bake in ONE example agent (swap for aider, codex or gemini as you like). hort stays
# agent-agnostic; this is just what *this* rootfs happens to ship.
RUN npm install -g @anthropic-ai/claude-code

# Inside a sandbox your session is root of its own user namespace: uid 0 in there, your own
# uid on the host, with no capabilities. Claude Code refuses to skip its permission prompts as
# root unless IS_SANDBOX=1 says it is in a sandbox, so set it in the rootfs. An ENV line would
# not survive the export, which keeps files and drops image settings, so write it where the
# login shell reads it. If your shell is fish, put `set -gx IS_SANDBOX 1` in /etc/fish/conf.d/.
RUN printf 'export IS_SANDBOX=1\n' > /etc/profile.d/hort-sandbox.sh

# /workdir is where hort bind-mounts the worktree. It must be writable by the mapped uid.
# Because that uid is arbitrary (contract #2), make it world-writable rather than chown-ing
# it to a fixed user. Remember the chmod after the export as well (see the top of this file).
RUN mkdir -p /workdir && chmod 1777 /workdir

# No USER line (contract #2): hort supplies the uid mapping via the rootless user namespace.
# No ENTRYPOINT/CMD that runs an agent (contract #5): hort injects the `sleep infinity` anchor.
# Leaving the base image's default command is fine; hort overrides init with the anchor.
