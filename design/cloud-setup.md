# Cloud session setup

A claude.ai/code session provisioned caos from four bash scripts totalling 1833
lines (735 of code): `setup.sh`, `install.sh`, `caos-pin.sh`, `session-start.sh`.
Three assumptions shaped that arrangement and **all three are false**, measured
2026-09-23. Correcting them made most of the machinery redundant and the rest two
stdlib-only Go programs run by `go run` (SPEC:425), plus a small third for the
session hook.

**This is built.** `bootstrap.go`, `install.go` and `session.go` replace the four
scripts, `tests/cloud-setup` covers them, and
`integrations/claude-code/cloud/README.md` documents the result. Two of this
plan's own proposals were refused on the way and then reached by a different
route; both detours are recorded below rather than tidied away, because the
reasoning that produced them is easy to repeat.

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
`caos-serve` — ran after Claude Code had launched and read its configuration.

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

## The arrangement

```sh
# the environment's setup field, in full
curl -fsSL "$B/integrations/claude-code/cloud/bootstrap.go" -o /tmp/caos-bootstrap.go
go run /tmp/caos-bootstrap.go --base="$B" --server="caos://<ticket>" \
    --dev-mode --enable-bash
```

`go1.24.7` is on the box at `/usr/local/go/bin/go`, with
`GOCACHE=/root/.cache/go-build`. `go run` on a single file with no `go.mod`
works and leaves no artifacts, **provided the program imports only the
stdlib** — a module fetch would have to reach `proxy.golang.org`, and the setup
phase is the phase that answers 503 for every n0 relay.

**Everything arrives as an argument, including the server ticket, and the ticket
is named ONCE.** The setup phase cannot read the environment's variables, so a
server named only there is a remote that appears after the tool server has
started without one. `--dev-mode` is a mode rather than a second server: it takes
the install package from the server `--server` already names.

### Two stages, because the installer is part of the payload

Stage 1 is the only thing that comes from GitHub, and it stays small and stable:

1. parse args; find the checkout; read the `caos` pin from `flake.lock`
2. download `git-remote-caos` from that pin's release — the ONLY reason a dev
   session touches a GitHub release, since git speaks `caos://` through it
3. fetch the payload: `refs/caos/dev` over the dev server, or the pinned
   release otherwise
4. `go run <payload>/install.go` with the same arguments

Stage 2 is the installer, and it comes FROM the payload — which is the whole
requirement dev mode exists for: an edit to the installer reaches the next
session with no push. The release carries the same file, so both modes run one
installer from two sources.

The pin read stays in stage 1 rather than becoming an argument. It is what lets
the two lines in the settings form name a branch and never be edited again: the
repository decides which caos, not the environment.

### The compile is the new cost

Cold `go run` of a stdlib-only program, measured locally, cache emptied between
runs:

| stage-1 shape | cold | cache |
|---|---|---|
| shells out to `curl`/`git` | **2.8s** | 40M |
| imports `net/http` | 5.7s | 95M |

Warm is 63ms. So **neither stage may import `net/http`** — it drags in the TLS
stack and doubles the compile, for something `curl` already does and `git` needs
anyway. Stage 2 then costs almost nothing, because stage 1 has just compiled the
same stdlib packages, so long as it imports the same set.

The container is fresh every session, so this is paid every session unless
`/root/.cache/go-build` ships warm in the image (see *Closed questions*).

### Size

Stage 1 and stage 2 replace `setup.sh`, `install.sh` and `caos-pin.sh` — 1388
lines of bash — with 1258 lines of Go, 945 of them code. The estimate was
450-550, and it was wrong in the direction worth knowing: the error messages
survived. They are most of what those scripts were, and they are why an
environment that is misconfigured says so instead of starting and behaving oddly.
`session.go` is 219 against `session-start.sh`'s 445.

**The win is not line count.** Go is wordier per operation. It is
`encoding/json` instead of a `jq` program and `@sh` quoting, errors instead of
`2>/dev/null`, and above all a program the suite can test — today nothing tests
these scripts but a four-minute cloud round trip.

## Two design changes to make at the same time

### Done, but only after a detour worth recording

The first attempt refused this, on a reading that was correct about the code and
wrong about what to do. `resolve_cli_image_with_store` resolves a
`--llm-step:@=<path>` by `t.ingest_path(".")` — the tracked working tree, dirty
edits included — and nothing about `mcp serve`'s `--base` reaches that walk. So
the seed commit decided what the CONVERSATION recorded while the worktree decided
which tools the server resolved, and rewriting only the seed would have produced
exactly the half-update the rewrite exists to prevent: a dev client evaluating the
committed tools.

The conclusion drawn from that — "the worktree rewrite stays" — treated the
client's resolution rule as fixed. It is not, and it was the thing that was
wrong: a recorded conversation has a base commit, and resolving its step anywhere
else is a gap between what a session runs and what it records, dev mode or not.
`resolve_cli_image_arg_in_tree` closes it, `mcp`'s two resolution sites pass the
base commit's tree, and stage 1 now builds the rewrite in a throwaway index only.

The checkout stays clean, and the Stop-hook friction below is gone with it.

The original reasoning follows, because it is what the change is for.

Setup used to
rewrite `.caos-expr` and `flake.lock` on disk so that `caos-std/`
resolves from the dev stack. That leaves the checkout dirty with a `caos://`
ticket, which is a credential. Observed in both test sessions: the client repo's
Stop hook demanded the changes be committed, and the agent had to reason its way
to declining — "it bakes an ephemeral dev-mode endpoint URL … into the repo, on
`main`". Correct judgement, but judgement, not a mechanism.

The seed commit was already minted through a throwaway index (`git commit-tree`,
unreferenced, so no push can carry it). Building the rewritten content only in
that index leaves the worktree untouched: the conversation seeds from the dev pin
through `caos mcp hook --base=<sha>`, the step resolves in that same commit, and
the ticket never enters a file anyone can commit. "Verify first that nothing
reads the worktree's expression directly" was the right instruction; something
did, and the answer was to change it.

**The bootstrap shrinks to `git-remote-caos`.** In dev mode the pinned GitHub
release is downloaded for one reason: the dev fetch speaks `caos://`, and git
speaks it only through that helper. Fetch the helper, not the whole client.

## What gets deleted

- `caos-pin.sh` — a flake.lock reader as its own file, because two bash callers
  needed the same answer. One Go program has no such problem.
- `session-start.sh`'s pin re-read and client refresh, and `caos-serve`'s
  curl-refresh: redundant per the third false assumption.
- `caos-serve`, and this went the other way round from the plan. The shim was
  what made `mcp.json`'s `args` dead, so what is deleted is the SHIM: the
  configuration now names `caos` with `mcp serve --llm-step:… --base=…` as
  ordinary argv, and there is no generated shell script in the arrangement at
  all. The shim existed only to re-install the client before exec'ing it, which
  the third false assumption paid for.
- The release resolution on the dev path (87 code lines skipped there today):
  stage 2 installs from a local directory in both modes, so there is one path.
- The comments. They narrate the journey rather than the decisions a reader
  might undo, against SPEC:426; 54-60% of these files is prose. The journey
  belongs in commit messages, which already carry it.

## Where the registry warm goes

It stays in the hook, and the reason was the NETWORK rather than the ordering.
The setup phase egresses through a TLS-terminating gateway that answers 503 for
all seven n0 relays, so a ticket carrying one of those was reachable only from a
session, and a warm moved into setup would have silently failed for every such
environment.

That premise has since been removed rather than answered: n0's relays are no
longer used at all, so every ticket now names a relay the operator runs and the
setup phase can reach it. The warm COULD move. It has not, because nothing has
measured it there, and an unmeasured move of the one step that decides whether a
first turn has tools is not an improvement.

The ordering argument was right and is not enough. What the hook keeps from it:
the warm marker is claimed BEFORE the warm starts, because Claude Code spawns the
tool server in parallel with the hook.

The remote and the unshallow did move into setup: neither needs a relay, and the
remote is what a session's first tool call cannot survive the absence of.

## Closed questions

Both of the measurements this plan wanted first were dropped as not worth a
session, and neither changes anything structural.

1. **Is `/root/.cache/go-build` warm in the container image?** Unmeasured. It
   matters less than it looked: the setup phase runs two Go programs, so by the
   time the hook runs a third, the packages it needs are compiled in that
   container whatever the image shipped. The worst case is one cold compile
   (~2.8s) against the ~15s the phase already costs.
2. **A third data point on per-session setup.** Not taken. Two consecutive
   sessions plus the platform's own `Running setup script` line in every
   `env_manager_log` is the evidence this rests on.

Two earlier questions are closed rather than answered. Whether setup can read
environment variables no longer matters, because everything is passed as an
argument. Whether the setup phase reaches `proxy.golang.org` no longer matters,
because the programs import only the stdlib — but if anyone ever adds a
dependency, that is the question they have to answer first.

## What remains for a `SessionStart` hook

`session.go`, and not much of it. Setup adds the `caos` remote and unshallows
before Claude Code starts; the hook warms the registry, reports a missing remote
without repairing it, and prints one line of stdout naming the build. Stdout,
because that is the only stream a session keeps: hook stderr is captured as a
non-transcript event and dropped.

## Testing

`tests/cloud-setup` is a `std/go` worker test over both stages: a fixture
checkout that pins caos, a release-shaped package, a dev-shaped tree, and
assertions on the installed tree, the written JSON, the rewritten expression and
lockfile, the seed commit's content and parent, and both stamps — including that
neither carries the ticket. No network, no server, no client.

It found two real defects on its first run, both of which a cloud session would
have reported as something else: stage 2 wrote its wrapper into a `bin/` it had
not created, and a home that does not exist was skipped silently, which would
have produced a session with a working client and no tools.

`std/go` gained `gitMinimal` for it. A Go worker that shells out to git is an
ordinary shape here, and the alternative was a second Go image.

Per step: `nix build` (the flake's `src` filter does not see what cargo sees),
`run-tool caos-test`, then one session. No environment bump is needed to pick up
a new `refs/caos/dev` — setup re-fetches it every session.

## What the settings form now holds

```
B=https://raw.githubusercontent.com/Metta-AI/caos/main
curl -fsSL "$B/integrations/claude-code/cloud/bootstrap.go" -o /tmp/caos-bootstrap.go
go run /tmp/caos-bootstrap.go --base="$B" --server=caos://<ticket>
```

`--dev-mode` is the third argument, and `--enable-bash` the
fourth. `CAOS_SERVER_URL` is not set at all. Deleting the four scripts means an
environment still pointing at
`setup.sh` gets a 404 — and `curl -f … | bash` exits ZERO on one, installing
nothing and reporting success — so the line has to change in the same breath as
the merge.

## The relay is required, and n0's are never used

Not a cleanup item — it came out of the cleanup breaking a live environment.
Bringing the stack up without `CAOS_IROH_RELAY` re-minted its ticket with an n0
relay: same endpoint id, same token, so the ticket sitting in the environment
still looked current, while the next session died in its setup phase with
`connecting to <id>: timed out`. The cause was a whole phase away from the
symptom, and nothing on either side said the relay had moved.

A default that can only ever be wrong is worth deleting rather than documenting.
So:

- `caos-iroh serve --relay <url>` is required, with no fallback.
- `caosd up --iroh` refuses to start without `CAOS_IROH_RELAY`, and says how to
  run one.
- the stack's bring-up refuses too, since it is where the message gets read.
- the endpoint builder uses `presets::Minimal` rather than `presets::N0`, so
  nothing reaches n0 for a relay, a pkarr publisher or a DNS lookup. A `caos://`
  ticket carries the endpoint id, the relay and the direct addresses, which is
  everything a dial needs.
- the client takes its relay map from the TICKET, so it no longer connects to an
  n0 relay as its own home relay before dialling anything.
- `proxy_from_env()` is gone from both ends. It existed to reach n0's TLS relays
  through a restrictive egress, and it actively broke a plaintext relay: iroh
  routes every relay dial through a configured proxy, so `http://<relay>/` was
  dialled as `CONNECT <host>:80` and timed out, while curl — which consults only
  `http_proxy` for an `http://` URL — went direct and answered 200.

What stays is the OS trust store (`CaTlsConfig::system()`), for an `https://`
relay behind a TLS-intercepting proxy.
