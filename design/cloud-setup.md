# Cloud session setup

A claude.ai/code session provisions caos from four bash scripts totalling 1833
lines (735 of code): `setup.sh`, `install.sh`, `caos-pin.sh`, `session-start.sh`.
Three assumptions shaped that arrangement and **all three are false**, measured
2026-09-23. Correcting them makes most of the machinery redundant and the rest a
single stdlib-only Go program, run by `go run` (SPEC:425).

Nothing in this document is built yet. The current arrangement is described in
`integrations/claude-code/cloud/README.md`.

## What actually happens when a session starts

One container per session, from the platform's `env_manager_log` plus in-session
`/proc/uptime`:

```
23:40:24  container boots
23:40:28  Cloned from seed bundle          (a fresh clone, every session)
23:40:29  Running setup script              (5s after boot)
23:40:43  Setup script completed            (14s of work)
23:40:43  Starting Claude Code
23:41:20  the model's first turn
```

**The setup script is the per-session entrypoint, and it is the only one that
precedes Claude Code.** Everything else caos owns — the `SessionStart` hook,
`caos-serve` — runs after Claude Code has launched and read its configuration.

## The three false assumptions

**"Setup runs once; the filesystem is snapshotted; later sessions skip it."**
This came from the docs and was never measured. A `touch` placed in the
environment's own setup field — outside every caos script, so nothing after
Claude Code starts can reach it — lands 5s after each container's own boot, in
two consecutive sessions with nothing changed between them:

| | run 1 | run 2 |
|---|---|---|
| container boot | 23:36:58 | 23:40:24 |
| `setup-touch` mtime | 23:37:03 | 23:40:29 |
| setup duration | 15s | 14s |

Whatever a cached environment caches, it is not script execution. For
`anthropic/ccr` the setup script runs every session.

**"Work after the snapshot is paid by every session, so do it in setup."** The
premise was right and the conclusion inverted: *everything* here is paid every
session, setup included. ~15s per session buys a GitHub release download, a
`refs/caos/dev` fetch and two installs.

**"The snapshot freezes what setup installed, so re-do it per session."** This
is the reason `session-start.sh` re-reads the pin and re-runs `install.sh`, and
the reason `caos-serve` curls `install.sh` before exec'ing the tool server.
Both redo work setup already did, in the same container, seconds earlier.

## The target

```sh
# the environment's setup field, in full
curl -fsSL "$B/integrations/claude-code/cloud/setup.go" -o /tmp/caos-setup.go
go run /tmp/caos-setup.go --base="$B" --dev-server="caos://<ticket>"
```

`go1.24.7` is on the box at `/usr/local/go/bin/go`, with
`GOCACHE=/root/.cache/go-build`. `go run` on a single file with no `go.mod`
works and leaves no artifacts, **provided the program imports only the
stdlib** — a module fetch would have to reach `proxy.golang.org`, and the setup
phase is the phase that answers 503 for every n0 relay.

One program: read the pin, fetch the package, install it, mint the conversation
seed, write the configuration, stamp what it did. It replaces `setup.sh`,
`install.sh` and `caos-pin.sh` — 1388 lines of bash — with an estimated 450-550
lines of Go.

**The win is not line count.** Go is wordier per operation. It is
`encoding/json` instead of a `jq` program and `@sh` quoting, errors instead of
`2>/dev/null`, and above all a program the suite can test — today nothing tests
these scripts but a four-minute cloud round trip.

## Two design changes to make at the same time

**The dev rewrite belongs in the seed commit, not on the worktree.** Setup
currently rewrites `.caos-expr` and `flake.lock` on disk so that `caos-std/`
resolves from the dev stack. That leaves the checkout dirty with a `caos://`
ticket, which is a credential. Observed in both test sessions: the client repo's
Stop hook demanded the changes be committed, and the agent had to reason its way
to declining — "it bakes an ephemeral dev-mode endpoint URL … into the repo, on
`main`". Correct judgement, but judgement, not a mechanism.

The seed commit is already minted through a throwaway index (`git commit-tree`,
unreferenced, so no push can carry it). Building the rewritten content only in
that index leaves the worktree untouched: the conversation still seeds from the
dev pin through `caos mcp hook --base=<sha>`, and the ticket never enters a file
anyone can commit. Verify first that nothing reads the worktree's expression
directly.

**The bootstrap shrinks to `git-remote-caos`.** In dev mode the pinned GitHub
release is downloaded for one reason: the dev fetch speaks `caos://`, and git
speaks it only through that helper. Fetch the helper, not the whole client.

## What gets deleted

- `caos-pin.sh` — a flake.lock reader as its own file, because two bash callers
  needed the same answer. One Go program has no such problem.
- `session-start.sh`'s pin re-read and client refresh, and `caos-serve`'s
  curl-refresh: redundant per the third false assumption.
- `mcp.json`'s step-locator rewrite. `install.sh` rewrites the locator into
  `args`, then sets `command: "caos-serve"` — a generated shim that execs
  `caos mcp serve` with its own arguments. Only a fallback branch for a build
  that cannot name its commit reads those `args`.
- `install.sh`'s release resolution whenever `--dev-assets` is set (87 code
  lines skipped on that path today).
- The comments. They narrate the journey rather than the decisions a reader
  might undo, against SPEC:426; 54-60% of these files is prose. The journey
  belongs in commit messages, which already carry it.

## Where the registry warm goes

Into setup. Today the `SessionStart` hook resolves `--llm-step` and fills the
cache `mcp serve` reads, racing the tool server Claude Code spawns in parallel
with the hook — and blocking the session until the hook's output stream reaches
EOF. In setup it simply precedes the tool server, so the first turn has its
tools rather than racing a resolve it cannot see.

## Open questions

1. **Can setup read the environment's variables?** Load-bearing, and the one
   measurement behind the current answer is suspect: a session stamped `off`
   while the environment set `CAOS_DEV=1`, read as "setup gets no env vars",
   from the same era as the snapshot assumption. If setup *can* read them,
   `--dev-server` stops being an argument and setup can add the `caos` remote
   from `CAOS_SERVER_URL` — which is most of what remains of the hook.
2. **Does the setup phase reach `proxy.golang.org`?** Only matters if a
   dependency is ever added. Record it as a constraint either way.
3. **A third data point on per-session setup**, hours later, environment
   untouched. Two consecutive sessions in one evening is thin evidence for
   deleting a refresh path.

(1) and (2) are one session together, and (1) decides how much of the hook
survives.

## What remains for a `SessionStart` hook

If setup can read the environment: the unshallow, and one stdout line naming the
build — stdout being the only stream a session keeps, since hook stderr is
classed non-transcript and dropped. Roughly 30 lines, or a second `go run`.

If it cannot: the `caos` git remote and the warm stay in the hook, because both
need `CAOS_SERVER_URL`.

## Testing

`tests/cloud-setup`, which does not exist in any form today: run the program
against a fixture checkout and a fixture asset directory, and assert the
installed tree, the written JSON, the seed commit's content and the stamp. Cloud
sessions then confirm integration instead of carrying all of it.

Per step: `nix build` (the flake's `src` filter does not see what cargo sees),
`run-tool caos-test`, then one session. No environment bump is needed to pick up
a new `refs/caos/dev` — setup re-fetches it every session.

## Order

Phase 0 (the open questions) → the seed-commit change and `setup.go` together →
the hook → the deletions. One commit each, on a branch off `main` taken after
`dev-mode-whole-package` lands.
