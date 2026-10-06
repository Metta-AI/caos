# Daemons — resident workers, supervised by the runner

**Status:** Parts 1 and 2 are implemented; Part 3 is still a sketch. The design
below is the plan this was built from, and "As built" says where the code differs.
Not done: converting the actor wrapper (`std/actor`) to a resident worker, which
the "Actors" paragraph under Keyed dispatch anticipated. Its start and finish are
two jobs per message, so the mailbox would not serialize a whole message and the
compare-and-swap would stay; that is worth redesigning, not just rewiring.

## As built

Where the code differs from, or settles, what the sections below say:

- **Where it lives.** The server's owner table and leases are in
  `rust/crates/server/src/runner.rs`; the runner state machine, `caos next` and
  its socket are in `rust/crates/caos/src/runner.rs` (the runner moved out of
  `bin/caos.rs`). `design/runner-protocol.md`, "Resident workers", is the wire
  protocol.
- **The ownership has its own id, `tenure`.** It is minted when a keyed job is
  claimed, rides in that job's payload, and is what a runner presents to renew
  its lease and to poll. The first claimant is usually runnerd's poll, which is
  not the container that will own the key, so the owner could not be named by a
  poll.
- **A result ends the ownership unless it says `keep`.** That is how "a worker
  that never calls `caos next` behaves exactly as today" is made true without the
  server knowing which workers are resident. `caos next` posts with `keep`.
- **An `idle` poll does not release the key.** The table below said it did; the
  owner polls again, and a job that arrived in between waited in its queue.
  Eviction (`exit`), a result without `keep`, an explicit release and a lease
  lapse end an ownership.
- **Salt is `/cas/salt`, not `/cas/args/salt`.** The arg is a lazy placeholder,
  and reading it needs a fetch that the file does not. The runner writes
  `/cas/salt` and `/cas/nonce` (root-owned, 0600) for a resident image, and
  `caos` reads them before it reads `CAOS_SALT` / `CAOS_JOB_NONCE`. The
  environment still carries the FIRST job's values, because scripts read it
  (`dev/cli-test/worker` re-points `CAOS_SALT`), so it is stale from a daemon's
  second job on. Only `/bin/caos` reads the files: a client that merely shares a
  container with a runner, like the test stack's `caos-cli`, would otherwise pick
  up the outer job's context.
- **Residency only starts at `caos next`.** An image can declare
  `CAOS_RESIDENT=1` and still run ordinary jobs: a worker that exits without ever
  calling `caos next` takes the runner back to warm polling, as on any image.
  `dev/test-stack` declares it and still runs `caos-build` and the suite's
  fan-out. The lifetime cap applies to a keyed life only, and counts from its
  start.
- **`caos next` exits 10 for "leave"**, 0 for a new job, 1 for an error
  (including a refusal). `--stream` writes a `job` line first, for the job already
  running, then one per later job; the worker answers `done` or `error <text>`.
- **Knobs:** `CAOS_LEASE_SECS` (15), `CAOS_LEASE_START_SECS` (600) on the server;
  `CAOS_LEASE_RENEW_MS` (5000), `CAOS_RESIDENT_POLL_MS` (10000) and
  `CAOS_NEXT_SOCKET` on the runner.
- **`start` does not reply early.** One job at a time per key means `status`
  would queue behind a `start` still bringing the stack up, so replying with the
  ticket before the stack is ready would not let anyone ask how far it got. It
  replies when the stack is ready and the listener has published its ticket.
- **`request-id`** replaced the actor wrapper's `nonce` in the actor code and its
  tests, and is the arg `caos-stack` requires.
- **The callers.** `std/caos-test` and `std/caos-stack` are routers on std/bash:
  they hash the tree, use it as both `affinity` and `in`, and tail-call the
  message at `dev/stack-daemon`. `caos-test` is cached like any job; `caos-stack`
  needs a fresh `request-id` per call or it answers from the cache.
- **Tests:** `tests/resident` (a resident image under `dev/resident-test`: one
  process for three concurrent messages, serialized, `/cas` content kept; an
  explicit stop; a new container after it; a killed worker fails only its job;
  `caos next` refused off a resident image and off a keyed job) and
  `tests/stack-daemon` (the ops that must not start a stack). The suite itself is
  the test of `start` and `run-tests`. Not tested end to end: eviction, the
  lifetime cap, a lapsed lease, `harvest`'s final run in the grace period and
  `--stream`; the server half of the first three is unit-tested.


Describes what to build on top of the runner as it exists today (SPEC.md,
"Runners: how a worker's container lives"; if that section is not in this
tree yet, it is Metta-AI/caos#311). Replaces the "Deferred: daemons" sketch in
`design/actors.md` on the `actors-design` branch, which put a separate
supervisor in the author's image. The supervisor already exists: it is
`caos runner`, PID 1 in every worker container.

Part 1 is the general mechanism. Part 2 uses it to run a caos stack that a
Claude cloud session can be driven against. Part 3 is a future extension: waking
a daemon from a connection.

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
- The server has no notion of a runner that is busy. A runner has a poll parked
  only between jobs, so while it is working the server cannot tell "busy" from
  "absent".
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
5. **The server decides who handles a message.** Only the server can tell a
   busy daemon from an absent one, so exclusion is its job. Workers and callers
   cannot be trusted to avoid races, because a race is exactly the case where
   they cannot see each other.
6. **Opt-in per image**, declared by the image author, because a resident
   worker can break the hermeticity that caching relies on.

## Two things that used to be called a nonce

- The **job nonce** is the server's rendezvous id for one claimed job. A runner
  receives it with the job and presents it back with the result and with every
  sub-run, which is what authorizes them. It is not in the ArgTree, so it never
  reaches a cache key, and nothing a caller writes can name it.
- A **`request-id`** is an ordinary ArgTree entry that a caller adds so that
  otherwise identical requests have different ArgTrees, and therefore different
  cache keys. It is the caller's answer to memoization, and the server gives it
  no meaning. (The actor wrapper used to call this arg `nonce`.)

This document says "nonce" only for the first. The job nonce is chosen by the
server and dies with the job; a `request-id` is chosen by the caller and is
cached with the result.

## The model

A **resident worker** is a `/worker` process that handles more than one job.
It does so by calling `caos next` when it finishes a job instead of exiting.
`caos next` means "I am done with this job; register me for another".

```
msg 1 ─▶ generic poll ─▶ runnerd ─▶ docker run … caos runner
                                     runner: set up /cas, fork /worker       (as today)
                                     server: this runner now OWNS key K       (new)
                                     worker: handle job 1, write /cas/out, `caos next`  ◀── blocks
                                     runner: post result 1, narrow reset, poll {base, affinity}
msg 2 ─▶ server: K has an owner ─▶ queued for it, never anywhere else
                                     runner: set up /cas/args for job 2, answer `caos next`
                                     worker: handle job 2 in the SAME process, `caos next` …
leave:   idle limit / eviction / stop / worker death ─▶ runner exits ─▶ ownership released ─▶ slot freed
```

- The **instance** is a reserved ArgTree entry, `affinity`. A job's key is the
  pair `(base, affinity)`, by oid.
- A worker that never calls `caos next` behaves exactly as today.

## Keyed dispatch (the server change)

The runner cannot enforce exclusion alone. A resident runner is parked only
between jobs, so a message that arrives while it is working finds no poll for
its key and would fall through to the generic pool, where a second container
would start. Retrying callers, an epoch in the start message and a daemon-side
check do not close this, because each of them still has to guess whether the
daemon is busy or gone. The server can know.

- **The key.** `affinity` is a reserved entry of the ArgTree, a blob, beside
  `base` and `salt`. Like any entry it is part of the cache key and of
  single-flight dedupe. A job without `affinity` is dispatched exactly as today.
- **The owner table.** Under the mutex that already guards the `parked` and
  `pending` tables, the server keeps `owners: (base, affinity oid) → {runner,
  lease expiry, FIFO queue}`.
- **Claim.** A keyed job with no owner is offered to any poll that matches it
  today: the generic pool, or a warm runner of the image. The poll that takes it
  becomes the owner in the same step. The check and the claim are one
  atomic step, so two concurrent first messages cannot both claim: the second
  finds an owner and queues.
- **Queue.** A keyed job whose key has an owner goes to that owner's queue and
  to nobody else, whether the owner is parked or busy. When the owner next
  parks, with `required` naming the same key, the head of the queue is answered
  immediately. A daemon therefore processes one job at a time, in arrival
  order, and is never idle while work waits.
- **Lease.** The poll names its runner. While a job runs, the runner renews a
  lease every few seconds from its own thread (a new `/runner/lease` call). A
  lapse, for example three missed renewals, ends ownership: the queue returns to
  the pending table, unowned, and the in-flight job fails with an error the
  caller can retry. A failure is never cached. This is the lease-based
  dead-worker detection that `runner.rs` notes as future work. Liveness comes
  from the worker's own traffic and is never inferred from slowness.
- **Release.** Ownership also ends when the owner's poll is answered `idle` or
  `exit` with an empty queue, or the runner posts its last result and exits. An
  owner with a queue is never answered `idle`.

What this gives:

- exactly one owner per key; no spillover, no duplicate daemons
- strict arrival order per key, with the server holding the mailbox
- dead-owner detection for keyed jobs
- no `--pin`, and no epoch to get exclusion. The cache still applies as before,
  so a message with effects still carries a `request-id`

The same mechanism fits actors (`design/actors.md`). Requests for one actor
serialize in the server's mailbox instead of racing on a compare-and-swap and
retrying.

What it costs: a real server change (the owner table, queues, matching rules,
the lease call and a runner heartbeat thread), and strictly one job at a time
per key. A cache hit never reaches the queue, because the cache is checked
before dispatch.

## `caos next`

A new subcommand of the setuid `/bin/caos`.

| form | meaning |
|---|---|
| `caos next [--error <text>]` | Finish the current job and block until the next one. Returns with the new args at `/cas/args`, or with a distinguished status meaning *leave*. `--error` fails this job with a message and continues |
| `caos next --stream` | For daemons that do not want a process per job. A long-lived helper: the runner writes one line per new job to its stdout, and the daemon writes `done` or `error <text>` to its stdin. EOF means leave |

Notes:

- The runner owns a root-only unix socket. `caos next` is the only client, so
  the unprivileged worker still reaches the runner only through the setuid
  binary, as it does for `/cas` today.
- A job with no `/cas/out` is a failed job, exactly as today (`read_result`
  fails). The runner posts the failure and carries on; the daemon does not have
  to die to report it.
- `caos next` is refused unless the image declares `CAOS_RESIDENT=1`.
- The key needs no declaration: it is the job's own `affinity` arg.

## What the runner does

| event | action |
|---|---|
| worker calls `caos next` (first time) | check the opt-in; note the job's `(base, affinity)` key |
| worker calls `caos next` | post the result; narrow reset (below); poll with `required = {base, affinity}` |
| poll returns a job | set up the job's `/cas/args`, `/cas/nonce`, `/secret`; answer `caos next` |
| poll returns `idle` | the server only does this when the queue is empty; poll again, unless the worker is gone or the lifetime cap is reached |
| poll returns `exit` (eviction) | tell the worker to leave; SIGTERM; wait out the grace period; SIGKILL; exit |
| a job is running | renew the lease on a timer |
| worker exits while a job is running | post a failure with the output tail; exit |
| worker exits between jobs | stop polling; exit (noticed within one poll TTL) |
| lifetime cap reached | same as eviction |
| runner or container dies | runnerd's existing backstop; the lease lapses and the server releases the key |

Poll TTL for a resident runner should be short (tens of seconds) and re-polled.
A dead runner's parked poll lingers until its TTL, but ownership is governed
by the lease, so queued jobs are not stuck behind it.

## Per-job context and reset

A long-lived process cannot have its environment changed per job, so per-job
context moves out of the environment.

- **Salt** is a reserved entry of the ArgTree, so it is at `/cas/args/salt`.
  The runner currently re-exports it as `CAOS_SALT` and `caos` reads only the
  environment (`run_salt()`). In a worker, `caos` should read `/cas/args/salt`
  first and fall back to the environment.
- **The job nonce** is the runner's rendezvous id and is not in the ArgTree. The
  runner writes it to a root-owned `/cas/nonce`, and `caos` reads it there in
  place of `CAOS_JOB_NONCE`.
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

## Routing and waking

- The first message for a key has no owner. The generic pool, or a warm
  `{base}` runner, takes it and becomes the owner. That worker brings the
  daemon up and calls `caos next`.
- Later messages for the key queue for the owner (Keyed dispatch, above).
- A message for a key whose daemon has gone arrives with no owner and wakes a
  new one. A daemon may choose not to start for some ops: for example, `status`
  and `logs` can answer *not running* and exit. That costs a container start, so
  it is a policy choice per daemon.
- Duplicate `start` messages are harmless. They queue behind the first, and the
  daemon answers the later ones with the current state. Callers do not need to
  agree on an epoch to get exclusion.

## Stopping

| cause | effect |
|---|---|
| eviction (`exit`) | the daemon is a cache of work, not a lease: it is told to leave, given a grace period, then killed. The next message wakes a new one |
| daemon exits | the runner stops polling and the container exits; the server releases the key |
| explicit stop op | the worker leaves its loop and exits normally; the full reset runs |
| lifetime cap | same as eviction; enforced by the runner, not trusted to the daemon |
| container or host loss | the lease lapses; runnerd's `caos.runnerd.owner` reaping on restart removes the container |

A resident container still occupies a runner slot. Eviction is what keeps it
from holding the slot against demand. Eviction applies only to a parked owner
whose queue is empty.

## Opt-in and limits

Declared by the image, read by the runner from its own environment:

- `CAOS_RESIDENT=1`: `caos next` is allowed
- `CAOS_RESIDENT_MAX_SECS`: hard lifetime cap
- `CAOS_RESIDENT_GRACE_SECS`: SIGTERM to SIGKILL

A caller cannot set any of these.

## Caching, hermeticity, secrets, state

- A message's result is cached by its ArgTree like any other job, and identical
  concurrent requests share one run (single-flight). A pure query can be
  answered from the cache without reaching the daemon. An op with effects
  carries a `request-id` arg, which is the existing answer to memoization. This
  includes `start`, which the daemon makes idempotent.
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
| server | `affinity` reserved entry; the owner table, claim, queues and release in the matcher; a lease call; lapse handling |
| `/bin/caos` | `next` subcommand; read salt from `/cas/args/salt`; read nonce from `/cas/nonce` |
| `caos runner` | socket and state machine above; narrow reset; continuous output capture; the lease heartbeat; liveness; caps |
| worker libraries | a small helper that wraps the `caos next` loop |
| runnerd | none |
| docs | SPEC.md runner section; `runner-protocol.md` "Resident worker daemon" |

## Not verified

- Whether a same-uid daemon can use `caos get` and `caos put` across jobs with a
  narrow reset. The `/cas` xattr model suggests so; no one has run it.
- Whether the salt entry is materialized at `/cas/args/salt` for every job, and
  every site that reads `CAOS_SALT` or `CAOS_JOB_NONCE` (only three were read).
- How keyed dispatch interacts with the pending timeout (a queued job should not
  expire while its owner is live), eviction, the seeded-sentinel logic, and the
  runner token and lineage rules in `runner.rs`.
- The lease: renewal interval, how many misses count as a lapse, and what a
  runner that is alive but slow to renew (a stalled thread) costs.
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
- **Exclusion without a server change** (an agreed epoch in `start`, retries,
  a daemon-side check for a second instance). Each one leaves the busy-versus-
  absent ambiguity open, so some message can still start a second daemon.
- **A registry and proxy that map connections to start messages.** Useful for
  wake-on-connect, and can live outside core caos. Part 3 returns to it.

## Build order

1. Spike: a trivial resident image whose worker loops on `caos next`. Check
   that job 2 reaches job 1's process (same pid), that a file fetched in job 1
   is still in `/cas` in job 2, and that eviction ends it. Do this against the
   current server with a single caller first, to separate runner questions from
   server ones.
2. Keyed dispatch in the server, with tests for: two concurrent first messages
   claim once, a busy owner queues rather than spills, arrival order is kept,
   release on `idle` and `exit`, and a lease lapse re-dispatches the queue and
   fails the in-flight job.
3. `caos next`, the socket and the runner state machine, with tests for:
   narrow reset contents, one failed job not ending the daemon, daemon death
   ending the runner, the lifetime cap, and a refused opt-in.
4. Per-job context: the salt and nonce readers.
5. Output capture and masking for a long-lived worker.
6. Update SPEC.md and `runner-protocol.md`.

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

Every message carries `affinity` (the instance name), `op`, and a `request-id`,
so it always reaches the daemon and is never answered from the cache.

| op | does | reply |
|---|---|---|
| `start` | bring the inner stack up if not already; needs `relay`; optional `publish` (publish std from the tree under test) | `{ticket, phase}` as soon as the iroh listener has published its ticket; the stack may still be coming up |
| `status` | phases from the stack's startup, listener state, uptime | text; *not running* if there is no daemon |
| `logs` | tail of the stack's member logs, from a cursor | text and the next cursor |
| `harvest` | export selected inner refs to the outer server | the refs written |
| `stop` | stop the stack; leave the loop | `stopped` |

`start` is idempotent. If the stack is already up it replies with the current
ticket, so racing or repeated starts are harmless: keyed dispatch queues them
behind the first. `status` and `logs` answer *not running* for a dead instance
rather than starting it.

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
  an image built from the tree under test cannot be granted one. Part 3 gives
  another route: an address derived from the ArgTree.

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
| `start` while a `start` or any job is running for the instance | queued behind it; replies with the current ticket |
| `start` after the stack was evicted or crashed | no owner, so it wakes a new stack with a new ticket |
| `status` or `logs` for a dead instance | a container starts, answers *not running*, and exits |
| stack crashes | the daemon exits, the runner exits, the key is released, the slot is freed |
| runner dies without exiting | the lease lapses; queued messages are re-dispatched and the in-flight one fails |
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

1. Part 1 steps 1 to 4.
2. `stack-daemon` image with `start`, `status`, `logs`, `stop`. Test: a second
   job reaches the same stack, and a stack started from the outer server answers
   a `caos-cli` call over its ticket.
3. `harvest` and the SIGTERM grace export.
4. The end-to-end agent flow with `drive`, as a scripted test against a fixture
   stack.

---

# Part 3 — Future: waking a daemon from a connection

**Status:** a sketch, not part of the first build.

Today a client must send a message before it can connect: Part 2's agent sends
`start`, reads the ticket from the reply, and only then points a session at it.
If a connection could name an ArgTree, the client could just connect, and the
daemon would spring into existence to handle it. Then `start` disappears.

## What a ticket and the iroh transport carry today

`crates/caos-iroh` speaks one iroh connection per client, with one bi-stream per
request. ALPN is `caos/1`. The opener writes a request line first:

```
caos1 <token> <service>[ <extra>]\n   ->   ok\n  |  err <message>\n
```

`caos1` is the magic first word, so a listener that gets something else on this
ALPN fails on the first line with a diagnosis. `<service>` is `http` (spliced to
the server), `git-upload-pack` or `git-receive-pack`. `<extra>` is one optional,
space-free field; for git it carries `GIT_PROTOCOL`.

A ticket is `caos://<EndpointTicket>.<token>`. `EndpointTicket` is iroh's own
type, an encoding of an endpoint address (id, relay and direct addresses). The
token is 32 bytes in hex. `Ticket::parse` splits from the right at the last `.`
and rejects a token that is not exactly 64 hex characters, so a field cannot be
appended to a ticket without breaking old parsers.

## Where an ArgTree can go

Not in iroh's `EndpointTicket`; caos does not own that type. Caos owns two other
places:

- **The request line.** A new service, for example `daemon`, with `<extra>` set
  to an ArgTree hash. A 40-hex hash has no spaces, so it fits the existing
  one-field `<extra>`.
- **The URL.** An optional route after the token, such as
  `caos://<endpoint>.<token>/<argtree>`. It is backward compatible if `Ticket`
  parses it and the client that writes the request line passes it along.

The address of a daemon then becomes computable from its ArgTree, with nothing
to start first. A ticket no longer has to be an output of `start`.

## How a connection would wake a daemon

An out-of-core listener or proxy beside the caos server:

1. receives a stream with service `daemon <argtree-hash>`
2. submits that ArgTree as a job. It already exists on the server, since only the
   hash travels. Concurrent connections submit the same ArgTree, and keyed
   dispatch (Part 1) makes the rest safe
3. waits for the reply, which carries the address the daemon listens on inside
   its container
4. splices the stream to that address

## Open issues

- **Authorization.** A connection that can name any ArgTree is the same
  capability as running arbitrary jobs, which the server token is today. A
  per-route token, `HMAC(master, hash)`, would limit a holder to waking one
  daemon. The listener currently holds a single token (`tokens_match`).
- **The ArgTree must already be on the server**, so someone has to push it first.
- **The reply must carry an address** the proxy can dial across the container
  network. Nothing returns one today.
- **Idle stop** stays with the daemon, or with the proxy if it tracks streams.
- **Cold-start latency** is paid by the connecting client, which must wait. A
  `git ls-remote` against a cold stack blocks for the time it takes to come up.
- **The client format** (`git-remote-caos`, `caos-cli`) must learn the route.
