# Daemons — resident workers, supervised by the runner

**Status:** proposal. Nothing here is implemented.

Describes what to build on top of the runner as it exists today (SPEC.md,
"Runners: how a worker's container lives"; if that section is not in this
tree yet, it is Metta-AI/caos#311). Replaces the "Deferred: daemons" sketch in
`design/actors.md` on the `actors-design` branch, which put a separate
supervisor in the author's image. The supervisor already exists: it is
`caos runner`, PID 1 in every worker container.

Part 1 is the general mechanism. Part 2 uses it to run a caos stack that a
Claude cloud session can be driven against.

---

# Part 1 — The general plan

## Problem

Some things want to outlive a job: a test stack, an HTTP server, a warm model.
Today every job ends with its worker process, and the container ends soon
after. A job that runs for the whole life of the service would work, but its
caller waits until the service dies, and `run-then` and `map-then` callers
cannot wait like that.

## What exists

- `caos runner` is PID 1 in every worker container. It takes a job, runs a
  fresh `/worker`, posts the result, then long-polls `/runner/poll` for another
  job for the same image (`required: {base: <oid>}`), and exits when the poll
  comes back `idle` or `exit`.
- After each job `reset_after_job` kills every process owned by the worker uid
  and empties `/tmp`, `/var/tmp` and `/dev/shm`.
- The server matches a job to a poll iff every required entry equals the job's
  top-level arg of that name. The most required keys wins; ties go to the most
  recently parked poll.
- An image declares what the container may do in its own environment
  (`CAOS_WORKER_UID`, `CAOS_RUNNER_TTL_MS`, `CAOS_GRANT_ENGINE_SOCKET`,
  `CAOS_GRANT_VOLUMES`). A caller cannot ask for these.

## Principles

1. **No start mechanism.** The server and runner have no concept of starting a
   daemon. A message is an ordinary job. If nothing is resident for its
   instance, the job's worker brings the daemon up. A daemon may still define a
   `start` op of its own (Part 2 does), but to caos it is just another message.
2. **Wake on message, stop on idle.** The server's existing eviction rule is
   the idle stop; the next message is the wake.
3. **The runner is the only poller and the only poster.** Registration logic
   lives in one place, `caos runner`, not in every worker language.
4. **The daemon's life is the long poll's life.** If the daemon dies, the
   runner stops polling and exits. If the runner is told to leave, the daemon
   is told to leave.
5. **Opt-in per image**, declared by the image author, because a resident
   worker can break the hermeticity that caching relies on.

## The model

A **resident worker** is a `/worker` process that handles more than one job.
It does so by calling `caos next` when it finishes a job instead of exiting.
`caos next` means "I am done with this job; register me for another".

```
msg 1 ─▶ generic poll ─▶ runnerd ─▶ docker run … caos runner
                                     runner: set up /cas, fork /worker       (as today)
                                     worker: handle job 1, write /cas/out, `caos next`  ◀── blocks
                                     runner: post result 1, narrow reset, poll {base, instance}
msg 2 ─▶ (matches that poll) ────────▶ runner: set up /cas/args for job 2, answer `caos next`
                                     worker: handle job 2 in the SAME process, `caos next` …
leave:   idle limit / eviction / stop / worker death ─▶ runner exits ─▶ container exits ─▶ slot freed
```

- The **instance** is a name carried as an ordinary arg. Jobs with the same
  `base` and `instance` oid go to the same container.
- A **pin** names the args a resident runner keeps in its poll. The worker
  declares them the first time it calls `caos next`.
- A worker that never calls `caos next` behaves exactly as today.

## `caos next`

A new subcommand of the setuid `/bin/caos`.

| form | meaning |
|---|---|
| `caos next [--pin <arg>]... [--error <text>]` | Finish the current job and block until the next one. Returns with the new args at `/cas/args`, or with a distinguished status meaning *leave*. `--pin` is honoured on the first call. `--error` fails this job with a message and continues |
| `caos next --stream` | For daemons that do not want a process per job. A long-lived helper: the runner writes one line per new job to its stdout, and the daemon writes `done` or `error <text>` to its stdin. EOF means leave |

Notes:

- The runner owns a root-only unix socket. `caos next` is the only client, so
  the unprivileged worker still reaches the runner only through the setuid
  binary, as it does for `/cas` today.
- A job with no `/cas/out` is a failed job, exactly as today (`read_result`
  fails). The runner posts the failure and carries on; the daemon does not have
  to die to report it.
- `caos next` is refused unless the image declares `CAOS_RESIDENT=1`.

## What the runner does

| event | action |
|---|---|
| worker calls `caos next` (first time) | check the opt-in; record pins; read each pin arg's oid from `/cas/args` (the way `base` is learned) |
| worker calls `caos next` | post the result; narrow reset (below); poll with `required = {base} ∪ {pin: oid}` |
| poll returns a job | set up the job's `/cas/args`, `/cas/nonce`, `/secret`; answer `caos next` |
| poll returns `idle` | poll again, unless the worker is gone or the lifetime cap is reached |
| poll returns `exit` (eviction) | tell the worker to leave; SIGTERM; wait out the grace period; SIGKILL; exit |
| worker exits while a job is running | post a failure with the output tail; exit |
| worker exits between jobs | stop polling; exit (noticed within one poll TTL) |
| lifetime cap reached | same as eviction |
| runner or container dies | runnerd's existing backstop; the poll lapses at its TTL |

Poll TTL for a resident runner should be short (tens of seconds) and re-polled.
A dead runner's parked poll lingers until its TTL, and a job matched to it
waits; a short TTL bounds that window.

## Per-job context and reset

A long-lived process cannot have its environment changed per job, so per-job
context moves out of the environment.

- **Salt** is a reserved entry of the ArgTree, so it is at `/cas/args/salt`.
  The runner currently re-exports it as `CAOS_SALT` and `caos` reads only the
  environment (`run_salt()`). In a worker, `caos` should read `/cas/args/salt`
  first and fall back to the environment.
- **Nonce** is the runner's rendezvous id and is not in the ArgTree. The runner
  writes it to a root-owned `/cas/nonce`, and `caos` reads it there in place of
  `CAOS_JOB_NONCE`.
- **Future work arrives in `/cas/args`**, like the first job's.

**Narrow reset**, run by the runner at `caos next`:

- removed: `/cas/args`, `/cas/out`, `/cas/out-trace`, `/cas/nonce`, `/secret`
- kept: the rest of `/cas` (fetched content is checked against its oid, so a
  stale entry is still correct for its oid), `/tmp`, `/var/tmp`, `/dev/shm`,
  and every process

The **full** reset (kill the worker uid, wipe the scratch dirs) still runs when
a worker exits without calling `caos next`.

**Output.** Today the runner captures a worker's output until exit, masks any
injected secret value, relays it to the container log and attaches it to a
failure. A resident worker does not exit, so the runner must capture
continuously: mask line by line, relay as it goes, and keep a bounded tail
attributed to the current job for a failure report.

## Routing, wake, and spillover

- The first message has no matching poll. The generic pool, or a warm `{base}`
  runner, takes it. That worker brings the daemon up and calls `caos next`.
- Later messages carry the same `instance` and match the resident poll, because
  the most required keys wins.
- **Spillover.** A resident runner handles one job at a time. While it is busy
  it has no parked poll, so a new message for its instance falls through to the
  generic pool and starts a second container. Mitigation for now: a daemon
  decides per op whether to wake itself. Reads (`status`, `logs`) answer
  *not running* and exit without starting anything; only ops that are meant to
  wake do so. A later server change could add an *exclusive pin* so that a job
  carrying a pinned arg only matches pinned polls.
- **Duplicate starts.** The server dedupes on the ArgTree hash: identical
  concurrent requests share one run (single-flight), and a later identical
  request is answered from the cache. So two starts with identical args never
  make two daemons. Two daemons need two *different* ArgTrees, which happens
  when callers put a random nonce in `start` to defeat the cache. `start`
  therefore carries an agreed `epoch` instead (see Caching, below).

## Stopping

| cause | effect |
|---|---|
| eviction (`exit`) | the daemon is a cache of work, not a lease: it is told to leave, given a grace period, then killed. The next message wakes a new one |
| daemon exits | the runner stops polling and the container exits |
| explicit stop op | the worker leaves its loop and exits normally; the full reset runs |
| lifetime cap | same as eviction; enforced by the runner, not trusted to the daemon |
| container or host loss | runnerd's `caos.runnerd.owner` reaping on restart; the slot is freed when the container goes |

A resident container still occupies a runner slot. Eviction is what keeps it
from holding the slot against demand.

## Opt-in and limits

Declared by the image, read by the runner from its own environment:

- `CAOS_RESIDENT=1`: `caos next` is allowed
- `CAOS_RESIDENT_MAX_SECS`: hard lifetime cap
- `CAOS_RESIDENT_GRACE_SECS`: SIGTERM to SIGKILL

A caller cannot set any of these.

## Caching, hermeticity, secrets, state

- A message's result is cached by its ArgTree like any other job. A pure query
  can be answered from the cache without reaching the daemon. An op with
  effects carries a `nonce` arg, which is the existing answer to memoization.
- `start` is the exception: it carries a generation number, `epoch`, that its
  callers agree on, instead of a random nonce. Identical starts then dedupe
  (single-flight while the start runs, cached afterwards), so each epoch has
  exactly one daemon, however many callers race. The cost is that the cached
  reply outlives the daemon: after an eviction or a crash, a repeat of the same
  `start` is a cache hit and runs nothing. A caller learns that from `status`
  (*not running*) and bumps the epoch to start again.
- A resident worker's result must still depend only on its ArgTree and the
  instance's *declared* state. Hidden in-memory state breaks cache
  correctness silently. That is the daemon author's responsibility, and the
  opt-in is the acknowledgement.
- Secrets are injected per job and removed at `caos next`. A daemon copies
  what it needs while its start job runs. A reply cannot carry a secret value:
  files added with `caos put` are checked for them.
- State that must survive the container needs a home outside it. Candidates:
  `CAOS_GRANT_VOLUMES`, an actor branch, or export to the outer server (see
  Part 2). How volumes are named and scoped has not been checked.

## What changes

| where | change |
|---|---|
| `/bin/caos` | `next` subcommand; read salt from `/cas/args/salt`; read nonce from `/cas/nonce` |
| `caos runner` | socket and state machine above; narrow reset; continuous output capture; pins; liveness; caps |
| worker libraries | a small helper that wraps the `caos next` loop |
| server | none |
| runnerd | none |
| docs | SPEC.md runner section; `runner-protocol.md` "Resident worker daemon" |

## Not verified

- Whether a same-uid daemon can use `caos get` and `caos put` across jobs with a
  narrow reset. The `/cas` xattr model suggests so; no one has run it.
- Whether the salt entry is materialized at `/cas/args/salt` for every job, and
  every site that reads `CAOS_SALT` or `CAOS_JOB_NONCE` (only three were read).
- What runnerd does with a container that lives for hours.
- The cost of `caos next` per job; `--stream` exists in case it matters.

## Alternatives considered

- **The daemon polls `/runner/poll` itself.** It would reimplement the runner
  (unpack, `/cas`, secrets, nonce, result posting, 410 handling) in every
  language. Rejected; `caos next` keeps that in one place.
- **The worker forks a daemon and exits.** The reaper kills it, and every
  message becomes a fresh worker that is a client of the daemon, which cannot
  use `caos get`. Needs a pid exemption and a private protocol.
- **A long job that replies when it dies.** No core change. Fine for an agent
  that starts it with `run_async`; unusable for `run-then` and `map-then`.
- **A registry and proxy that map connections to start messages.** Useful for
  wake-on-connect, and can live outside core caos. Not needed for this design.

## Build order

1. Spike: a trivial resident image whose worker loops on `caos next`. Check
   that job 2 reaches job 1's process (same pid), that a file fetched in job 1
   is still in `/cas` in job 2, and that eviction ends it.
2. `caos next`, the socket and the runner state machine, with tests for:
   narrow reset contents, one failed job not ending the daemon, daemon death
   ending the runner, the lifetime cap, and a refused opt-in.
3. Per-job context: the salt and nonce readers.
4. Output capture and masking for a long-lived worker.
5. Update SPEC.md and `runner-protocol.md`.

---

# Part 2 — Running a caos stack and driving traffic against it

## Goal

From a cloud session (an agent), start a caos stack built from a tree under
test, point a Claude cloud session at it, drive traffic, collect the results,
and stop it. The agent does all of it with caos jobs and `drive`.

```
agent ── start/status/logs/harvest/stop ──job──▶ resident `stack-daemon` container (one per instance)
                                                    inner stack: server, runnerd, redis, git, iroh listener
agent ── drive env-create/start/send ──▶ Anthropic API ──▶ cloud session
                                              │  caos://<ticket>
                                              └──▶ private iroh relay ──▶ inner stack
```

## The image

A `stack-daemon` image, built from the root flake alongside `caosImage` and
reusing its interpreter (`design/test-stack-image.md`):

- the interpreter brings up the inner stack exactly as it does for a test, but
  its `worker1` is a daemon loop instead of a test run. Inside the loop it
  calls `caos next`
- it keeps the "two caoses" rule: `caos next` is the outer `/bin/caos`; the
  tested client at `/caos/bin` is used only at call sites
- image env: `CAOS_RESIDENT=1`, `CAOS_RESIDENT_MAX_SECS`,
  `CAOS_RESIDENT_GRACE_SECS`, plus what the test stack already declares
  (`CAOS_GRANT_ENGINE_SOCKET=1`, `CAOS_WORKER_UID=0`)

## Messages

Every message carries `instance` (the pin) and `op`. Every op except `start`
also carries a `nonce`, so it always reaches the daemon. `start` carries an
`epoch` instead (Part 1, Caching).

| op | does | reply |
|---|---|---|
| `start` | bring the inner stack up if not already; needs `relay`; optional `publish` (publish std from the tree under test) | `{ticket, phase}` as soon as the iroh listener has published its ticket; the stack may still be coming up |
| `status` | phases from the stack's startup, listener state, uptime | text; *not running* if there is no daemon |
| `logs` | tail of the stack's member logs, from a cursor | text and the next cursor |
| `harvest` | export selected inner refs to the outer server | the refs written |
| `stop` | stop the stack; leave the loop | `stopped` |

Reads never wake a stack (see Spillover above). `start` is idempotent per
epoch: the server runs one and answers repeats with the same result, which has
the same ticket. If `status` says *not running*, bump the epoch and start again.

## Identity and tickets

- The inner stack generates its own iroh identity. `stack/serve` keeps the key
  and token in `CAOS_STACK_IROH_STATE` and creates them on first use, so the
  daemon needs no secret and takes no ticket as input.
- The ticket is the `start` reply. Because the daemon replies and keeps running,
  nothing has to leave the container through a side channel.
- The reply, and so the token, is stored in the outer server as a result.
  That is acceptable here: the stack is disposable, holds no secrets, and
  everyone who can read the outer server's objects can already run jobs on it.
- A stack's address does not survive a restart. If it ever needs to, the iroh
  state directory can live on a granted volume, or the key can come from a
  secret held by pinned trusted code. Secrets are granted by code identity, and
  an image built from the tree under test cannot be granted one.

## Network prerequisites

- **A relay of your own**, on port 80 or 443, reachable from the container and
  from cloud sessions. `stack/serve` refuses to start the listener without
  `CAOS_IROH_RELAY`, and n0's relays are never used. Passed as the `relay` arg.
- **The engine socket**, offered by the outer pool (`CAOS_RUNNER_SOCKET`),
  which the image's `CAOS_GRANT_ENGINE_SOCKET=1` then claims.
- **The shared registry** (`caos-registry:5000`), as in `test-stack-image.md`.
- Behind docker NAT the listener is relayed, not direct, unless addresses are
  advertised (`CAOS_STACK_IROH_ADVERTISE`). Relayed is slower and works.

## Driving traffic

1. Send `start` with `run_async` (or `spawn_agent`-style async). The reply has
   the ticket.
2. Create an environment whose setup script names the stack:
   `drive env-create --from <template> --init-script <script>`, where the script
   is the two-line bootstrap from `integrations/claude-code/cloud/README.md`
   with `--server=caos://<ticket>`. One environment per instance.
3. Start a session with `drive start --env <env> --repo <client repo>
   --prompt <…>`, and continue with `drive send`.
4. Watch it: `drive info` and `drive conv` for the session; `status` and `logs`
   for the stack.
5. Collect results with `harvest`, then read them with `log`, `show` and
   `read` from the outer conversation.
6. Clean up: `drive archive`, `drive env-delete`, then `stop`.

The session's caos tools resolve against the inner stack, which is built from the
tree under test and has std published from it. That is the thing being tested.
`drive` needs the `claude-oauth-token` secret granted to it, and that token is
a monthly chore. The session uses the pinned release client, not a client built
from the tree under test. Pointing a session at a tree-under-test client (the
existing dev mode, `refs/caos/dev` and `--dev-commit`) is future work.

## Results: harvest

The cloud session records its conversation into the inner stack's git, and that
dies with the container. `harvest` copies it out.

- The daemon script, running with the outer `/bin/caos`, reads the inner stack's
  git directory in the same container and pushes selected refs to the outer
  server under `refs/stacks/<instance>/`. The tested code never gets outer
  write access.
- It runs on demand and once more during the SIGTERM grace period, so an
  eviction or a lifetime cap loses little.
- The inner stack keeps its own redis and its own result cache. Sharing them
  would share results, and a stack under test must not be able to write results
  that the outer server then serves.

## Lifetime and capacity

- Each stack holds one outer runner slot for its life. `test-stack-image.md`
  records what happens when a pool fills with long-lived stacks: queued work
  dies on the pending timeout. Cap concurrent instances, and warm the registry
  once before starting many (the `warm-std` pattern there).
- Eviction kills a stack. That is the intended behaviour, and the reason
  `harvest` runs during the grace period.
- Cold start dominates: about 20 s on a warm registry, minutes on a cold one.
  `start` replies early; `status` shows progress.

## Failure modes

| failure | result |
|---|---|
| relay unreachable or not set | `start` replies with an error; no ticket is published |
| engine socket not offered by the pool | the inner stack cannot start containers; `status` shows it |
| second `start`, same epoch | deduped by the server: one run, same reply and ticket |
| `start` with a different epoch while the first stack is up | a different request, so a second stack starts. Callers agree on the epoch |
| repeat `start` after eviction, same epoch | a cache hit; nothing runs. `status` says *not running*; bump the epoch |
| stack crashes | the daemon exits, the runner exits, the slot is freed; `status` says *not running* |
| eviction or cap | SIGTERM, final `harvest`, then killed |

## Not verified

- That the pool this session's server uses offers `CAOS_RUNNER_SOCKET`.
- Whether the interpreter's bring-up can run as a daemon's worker1 without
  changes, and how `CAOS_STACK_PUBLISH` behaves in that mode.
- How `harvest` should push from inside the container: the actors-style scratch
  repository with a lease push is the likely route, and it has not been tried.
- Whether a cloud session's setup phase can reach the private relay (the n0
  relays are refused there; a relay of your own on 80 or 443 should be fine).
- Volume scoping for any state that must outlive a stack.

## Build order

1. Part 1 steps 1 to 3.
2. `stack-daemon` image with `start`, `status`, `logs`, `stop`. Test: a second
   job reaches the same stack, and a stack started from the outer server answers
   a `caos-cli` call over its ticket.
3. `harvest` and the SIGTERM grace export.
4. The end-to-end agent flow with `drive`, as a scripted test against a fixture
   stack.
