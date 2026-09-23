# Git inside the sandbox

A sandbox gets its `/workdir` in one of two ways, and the choice decides whether git works inside it.

| | `worktree` (the default) | `clone` |
| :--- | :--- | :--- |
| What `/workdir` is | a git worktree of your repository | a clone of your repository |
| git inside the sandbox | does not work | works |
| Who commits | you, on the host | the agent, inside |
| Who pushes | you, on the host | the agent, with a token you forward |
| Your repository written from inside | never | never |
| Disk | the checkout | the checkout, plus a small `.git`; history is borrowed, not copied |

Worktree mode is the default and is the right one when you review and commit the work yourself. Clone mode exists for the case where the agent has to finish the job on its own: commit, push and open a pull request without you in the loop.

In both modes your repository is safe from what happens inside the box. The difference is only how the work comes back.

## Worktree mode

`/workdir` is a worktree on its own branch, and its `.git` is a pointer file naming your repository on the host, a path the sandbox does not have. So no git command works inside, by design, and you commit from the host while the sandbox runs. [Concepts](concepts.md#git-is-a-host-activity) describes it in full.

## Clone mode

### Turning it on

Per sandbox:

```bash
hort up fix-login --git clone
```

Or for a project, in `.hort.json` (or globally in `~/.config/hort/config.json`):

```jsonc
{
  "git": "clone",
}
```

The flag beats the configuration, so a project configured for clone mode can still be built as a worktree with `--git worktree`, and the other way round.

In a project that is not a git repository there is nothing to clone. The flag is refused:

```text
--git clone requires a git repository, but this project is not one
```

while the same mode coming from a configuration file only warns, and the sandbox is built the way a project without git always is, with the folder itself mounted:

```text
warning: the configured 'clone' git mode needs a git repository and this project is not one, so the sandbox mounts the project folder itself
```

The difference is deliberate: a flag is a request for this one build, while a global configuration file covers every directory on your machine, and refusing there would break `hort up` in every folder that is not a repository.

### What the sandbox gets

`/workdir` is a clone of your repository, with a real writable `.git` directory of its own. The branch rule is the same as in worktree mode: `hort up <name>` creates and checks out a branch named `<name>`, and `--branch <existing>` checks out one that already exists.

The git binary that works on it comes from the rootfs, so a rootfs without git leaves the clone unusable from inside (see [Preparing a rootfs](rootfs.md#what-a-rootfs-must-provide)).

Inside, the clone has two remotes:

| Remote | Points at | Push |
| :--- | :--- | :--- |
| `origin` | your repository's own `origin`, for example your GitHub remote | yes, this is where the agent's work goes |
| `hort-base` | your repository on the host | **no**, fetch only |

If your repository has no remote, the clone has no `origin` and you add one yourself.

`hort-base` being fetch only is a guarantee and not tidiness. A plain `git clone --shared` can push a new branch straight into the repository it was cloned from, so hort gives that remote a push address that cannot resolve. A push through it fails and your repository is never written.

The clone does not copy your history. It borrows your repository's object store, mounted **read-only** inside the sandbox at `/run/hort/objects`, which is why a clone of a large repository costs about as much disk as a worktree does. Read-only is enforced by the kernel:

```text
$ touch /run/hort/objects/anything
touch: cannot touch '/run/hort/objects/anything': Read-only file system
```

So the agent reads all of your history and rewrites none of it. After an agent commits inside a clone-mode sandbox, your repository's `.git` is byte for byte what it was.

### The pinned ref

Because the clone borrows objects instead of copying them, a `git gc` in your repository could in principle delete an object the clone still needs. To prevent that, `hort up` writes a ref in **your** repository naming the commit the clone started from:

```text
refs/hort/<name>/base
```

It is the one thing hort writes in your repository, it holds no work, and it is invisible to `git branch`. `hort down` deletes it along with the sandbox (see [what `hort down` does with a clone](#what-hort-down-does-with-a-clone)).

### Sending the work back

The agent commits in the clone and pushes to `origin`, then opens a pull request. Two things have to be in place for that:

- **Credentials inside the box.** Forward a token the way the [GitHub CLI recipe](recipes.md#github-cli-without-logging-in-each-time) does. Use a **fine-grained** token scoped to the one repository, with branch protection on the remote as the backstop: the token lives inside a box with an agent you are not watching.
- **Egress, if you use an allowlist.** hort never adds a host to your allowlist by itself, so list `github.com` (and `api.github.com` for `gh`) yourself. Under open egress they are reachable already.

If you would rather not give the box a token at all, have the sandbox write a bundle and take that instead. The bundle has to be made **inside** the box, where the borrowed history resolves, and `/workdir` is on your host, so it lands where you can reach it:

```bash
hort run fix-login -- git -C /workdir bundle create /workdir/work.bundle fix-login
git fetch ~/.local/state/hort/sandboxes/fix-login/worktree-fix-login/work.bundle \
    'fix-login:refs/hort/incoming'
```

Fetching from the clone **directory** does not work, for the same reason git run there does not (see below): it aborts with `remote: aborting due to possible repository corruption on the remote side`. The bundle is the way out.

### Reading the clone from the host

Running git from the host inside the clone directory fails:

```text
$ git -C ~/.local/state/hort/sandboxes/fix-login/worktree-fix-login status
error: unable to normalize alternate object path: /run/hort/objects
fatal: bad object HEAD
```

That is not damage. The clone records the address where it borrows objects, and that address exists inside the sandbox, not on your host. Ask the sandbox instead:

```bash
hort run fix-login -- git -C /workdir status
hort run fix-login -- git -C /workdir log --oneline -5
```

The same limit applies to fetching from that directory, which is why work comes back through a push or [a bundle](#sending-the-work-back).

### What is not there yet

Clone mode works, and one rough edge remains:

- **`hort ls` does not say which mode a sandbox was built in.** Its dirty column also reads `-` for a clone, because that check asks your repository about a worktree and a clone is not one. The branch column is correct.

It is on the [Roadmap](roadmap.md).

### What `hort down` does with a clone

`down` removes the clone along with the sandbox, and a commit that never left the box goes with it. So before it removes anything it compares the clone's tip against your repository and asks when your repository does not have it:

```text
sandbox 'fix-login' holds commits the project repository does not have; tear it down anyway? [y/N]
```

Anything but `y` leaves the sandbox standing. `hort down -f` skips the question, and without a terminal `down` refuses rather than guess. See [hort down](commands/down.md#unreturned-work).

It also deletes that sandbox's pinned ref `refs/hort/<name>/base`, the one your repository was holding so its gc would leave the clone's borrowed objects alone. The pins of your other clone-mode sandboxes stay where they are.

### Changing the mode of a sandbox that exists

A sandbox is built one way or the other, and `hort up` refuses to finish a half-built one in the other mode:

```text
sandbox 'fix-login' was built in clone git mode, not worktree — run 'hort down fix-login' first, or repeat it with 'hort up fix-login --git clone'
```

Take what you want out of it first, then tear it down and build again.

## Which one to use

Use **worktree mode** when you are the one reviewing and committing, which is most of the time. It is the default, it has fewer moving parts, and nothing inside the box can run git at all.

Use **clone mode** when an agent has to deliver finished work by itself, typically several agents in parallel each opening its own pull request. The cost is that a token now lives inside the box, so scope it narrowly and protect the branches on the remote.
