# example.Dockerfile — a starting point for a hort base rootfs.
#
# hort does NOT run this image. It consumes a prepared rootfs *directory* (foundation §13).
# This Dockerfile is a convenient way to *describe* the environment; you build it and then
# EXPORT it to a directory, which is what hort mounts as the read-only overlay lower layer:
#
#   podman build -t hort-devbox -f example.Dockerfile .
#   cid=$(podman create hort-devbox)
#   mkdir -p ~/.local/share/hort/rootfs/devbox
#   podman export "$cid" | tar -x -C ~/.local/share/hort/rootfs/devbox
#   podman rm "$cid"
#   # then point .hort.json / ~/.config/hort/config.json "rootfs" at that directory.
#
# (debootstrap or `umoci unpack` are equally valid ways to materialize the directory.)
#
# The contract this rootfs must honor (foundation §13):
#   1. Provide a shell — the first session and every `hort attach` drop into it.
#   2. Tolerate an ARBITRARY uid — hort runs rootless and maps the in-container user to the
#      host uid that owns the worktree, so NOTHING may hardcode a USER or assume a fixed uid.
#   3. Bake in the agent binaries you run — hort never installs agents.
#   4. Do NOT define a CMD/ENTRYPOINT that runs an agent — hort injects `sleep infinity` as
#      the container's init (the anchor). A CMD here would be ignored at best, harmful at worst.

FROM debian:bookworm-slim

# A shell + the basics agents and dotfiles expect. ca-certificates is needed for TLS egress.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        less \
        nodejs \
        npm \
    && rm -rf /var/lib/apt/lists/*

# Bake in ONE example agent (swap for aider / codex / gemini as you like). hort stays
# agent-agnostic — this is just what *this* rootfs happens to ship.
RUN npm install -g @anthropic-ai/claude-code

# /workdir is where hort bind-mounts the worktree. It must be writable by the mapped uid.
# Because that uid is arbitrary (contract #2), make it world-writable rather than chown-ing
# to a fixed user — this is exactly what the rootfs-validation check looks for (foundation §13:
# "/workdir is not writable by the mapped uid").
RUN mkdir -p /workdir && chmod 1777 /workdir

# No USER line (contract #2): hort supplies the uid mapping via the rootless user namespace.
# No ENTRYPOINT/CMD that runs an agent (contract #4): hort injects the `sleep infinity` anchor.
# Leaving the default shell as the image command is fine — hort overrides init with the anchor.
