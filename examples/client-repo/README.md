# A caos client repo

The tree an agent session starts from when it works through caos. It holds no
code of its own: a pin saying which caos to use, an expression mounting caos'
`std/`, the instructions the agent reads, and whatever tools you choose to add.

**The repository you actually want to change is imported into the
conversation, not cloned here.** That is the point of the arrangement. The
caos server fetches it from GitHub directly, so starting work on a large
repository costs no clone, no history fetch and no upload — a session on a
100k-commit repo begins as fast as one on an empty repo.

## Using it

Fork this directory into a repository of its own, then point a Claude Code
cloud environment at it:

- **Repository**: your fork.
- **Setup script**: two lines, which never change again —

  ```
  B=https://raw.githubusercontent.com/Metta-AI/caos/main
  curl -fsSL "$B/integrations/claude-code/cloud/setup.sh" | bash -s -- --base="$B"
  ```

  This `--base` only says where the *bootstrap scripts* come from. The caos
  that actually gets installed is the one **your fork pins**, which the setup
  script reads out of `flake.lock` before installing anything.

- **Environment variables**:
  - `CAOS_SERVER_URL` — required. The `caos://…` ticket `caosd ticket` prints
    on the machine running your server. It is a credential: whoever holds it
    can drive that server.
  - `GITHUB_TOKEN` and `CAOS_GITHUB_TOKEN_ENTROPY` — only for private
    repositories. See `.caos-secrets/github-token`.

Then start a session and say what to work on: *"import owner/repo and fix the
flaky test in its scheduler"*.

## Moving the pin

```sh
nix flake update caos
```

then set the two `rev=` values in `.caos-expr` to the commit `flake.lock` now
records. They must match: `std/flake-input-loader` compares them and refuses
the evaluation if they differ, naming both revisions. The next session picks
up the new client, the new tools and the new tree together — the session hook
re-reads this pin every time, so a pushed change reaches an existing
environment without rebuilding it.

**Pin a commit that already has a published build.** The client is downloaded
from that commit's release, while the tools resolve through the same rev, so
the two are one choice — and a commit whose build has not landed is refused
rather than paired with an older client. If you are moving the pin to your own
caos branch: push it, wait for the workflow to publish `build-<commit>`, and
re-pin after. `gh run list --branch <branch>` says when.

## What each file does

```
flake.nix / flake.lock   the pin: which caos, by commit
.caos-expr               mounts caos' std/ at caos-std/ (evaluated only, never on disk)
AGENTS.md                what the agent is told at the start of every session
.caos-secrets/           secret DECLARATIONS — names and readers, no values
.gitignore               /caos-std/, which must not exist as a real directory
```

Add your own `caos-tools/` here and the agent is offered them alongside the
standard set.
