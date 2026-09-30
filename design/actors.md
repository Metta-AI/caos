# Actors and daemons — persistent state in Git, single writer by compare-and-swap

**Status:** proposal. Nothing here is implemented.

Builds on [client-owned conversation refs](client-owned-conversation-refs.md)
(the Git protocol workers already use for conversation heads) and the
[runner protocol](runner-protocol.md) (warm runners, and the deferred "resident
worker daemon").

---

## Problem

caos runs pure, cached, hermetic jobs to completion. We also want things that
live across requests:

- a **test stack** that caos starts itself and that later conversations drive,
- assorted small servers whose state must outlive any one job.

Today a job that needs a daemon starts it as a child of its own container
(`std/caos-test`, `tests/remote-ref`); the daemon dies when the job returns.

## Model

An **actor** is a worker with a durable name and its state in a Git branch.
Cloudflare Durable Objects are the closest analogue. Three points define it:

1. **No Start message.** The container comes alive on the first request and
   exits after an idle period. So every request names the actor, and the
   worker finds the state itself.
2. **State lives on a branch.** The request carries the branch name. The worker
   reads the branch, handles the request, and pushes the new head.
3. **Concurrency control is Git's.** A push is a compare-and-swap on the ref
   (`--force-with-lease=<ref>:<observed>`). A writer that loses the race fails
   and retries, or fails the request. This is optimistic STM with commits as
   the transaction log.

Nothing in the server changes for this. An actor is a convention for workers
(see [What caos changes](#what-caos-changes)). Three flavours:

| flavour | process | when |
|---|---|---|
| **pure** | one-shot worker per request | state is data; handler has no external effects |
| **effectful** | one-shot worker per request | handler acts on the outside world; needs a write-ahead claim |
| **daemonic** | container that registers as a runner and stays up until idle | performance, or a live process (a test stack) |

## Research: what the existing code already gives us

### 1. Compare-and-swap on the server's Git transport — supported, in use

- The server delegates smart-HTTP to `git http-backend`
  (`rust/crates/server/src/git.rs`). Its repo config is set at startup in
  `main.rs` (`http.receivepack`, `receive.fsckObjects`, …). It does **not** set
  `receive.denyNonFastForwards`.
- That is fine. The expected-old-value check comes from the push command, which
  receive-pack applies under the ref lock. `--force-with-lease=<ref>:<observed>`
  therefore gives a server-side atomic CAS. **Fast-forward-ness is not enforced
  by the server**; the actor enforces it by always building its commit on the
  observed head. This matches the stance of
  [client-owned conversation refs](client-owned-conversation-refs.md): "the
  server provides transport, not policy."
- That design already specifies the exact client protocol we need: a scratch
  repo whose `origin` is `CAOS_SERVER_URL`; read with an exact-ref fetch;
  append with `git push --force-with-lease=<ref>:<observed> <new>:<ref>`; several
  refs at once with `--atomic`; after an ambiguous failure, **fetch again** and
  treat "my object is visible" as success, "head changed" as a lost race, and
  "head unchanged" as an infrastructure failure. Actors reuse this verbatim.
- Git is opt-in: only workers bound to the `git-runner` image have `git`.
  Actor workers select it in their `.caos-expr`, so it shows up in the
  ArgTree like any other dependency.
- `/git/push` (`push.rs`) is a different thing: it publishes a pinned commit to
  an *external* remote, with `expected` and `Complete/Conflict/Uncertain`
  receipts. Actors do not use it, but `Uncertain` is the same ambiguity we have
  to handle.

**Caveats.**

- The Git paths are **unauthenticated**. `handle()` in `main.rs` routes them
  before anything else, and only `/runner/*` checks a token. Any worker (or
  anyone who can reach the server) can rewrite an actor branch. We accept that
  today for conversation refs. Actors inherit it, so an actor branch is
  integrity-protected only against *accidents*, not against a hostile worker.
- Git advertises every ref on every push and fetch. Stale `refs/caos/req/*`
  are swept every 10 minutes for exactly this reason. Actor refs are
  permanent, so the number of actors is a (soft) scaling limit.
- GC is deliberately off, so actor history is never reclaimed. See
  [Open questions](#open-questions).

### 2. Are failures cached? — no; successes are, forever

In `compute.rs` (`run_dispatch_inner`), only an `Ok` result is passed to
`cache_set`. An `Err` is returned without caching, and a result that folded in
a caught sub-run failure is explicitly not cached. So:

- **Retrying a failed request with the same request ID re-runs it.** Good: a
  lost CAS race can simply be a failed job that the caller retries unchanged.
- **Retrying a succeeded request with the same ID is a cache hit** and replays
  the reply without running the worker. That is the idempotent replay we
  wanted — but it is **best-effort only**. The cache is Redis `SET` with no
  expiry, but a lookup error "just means we run uncached", and the key is
  namespaced by `cache_namespace`, so a stack change silently empties it.
  **Exactly-once therefore must not depend on the cache.** The actor records
  the request IDs it has applied, in its own state (see below).
- Single-flight (in-memory, per server) coalesces *concurrent* identical
  requests into one run, and a waiter never re-runs "merely because a valid run
  is slow". That is useful for duplicate delivery but is not a substitute for
  the state-based dedupe, since it does not survive a server restart.

### 3. Credentials for the branch — none needed in-stack

Because Git transport is unauthenticated (caveat above), an actor worker needs
only `CAOS_SERVER_URL`, which every worker already has
(`run-and-update-ref/src/refs.rs` reads it). Credentials are needed only for
*external* remotes or services, and those use the existing secret store
(SPEC.md "secrets": injected only when the worker's arg tree is a superset of
the secret's reader *and* carries the matching `secret-hash`). Two consequences:

- Secrets are injected out of band and never enter the cache key, so an actor's
  external credentials do not perturb request caching.
- If we later authenticate Git pushes, actor branches should be the first
  namespace to get per-branch writers.

### 4. Runner routing — already symmetric

`runner.rs::matches` is pure oid equality in both directions. A job entry whose
name starts with `REQUIRED_ARG_PREFIX` must equal the runner's entry of the
same name; conversely every entry a runner requires must equal the job's. A
job carrying `required-actor=<oid>` therefore reaches only a runner that polls
with `required: {required-actor: <oid>}`, and never leaks to the generic pool.
This is the routing half of daemonic actors and needs no server change. (Note
the header of `runner-protocol.md` still says "not yet implemented"; the
server side is in `runner.rs`.)

## Request contract

Every request to an actor carries, in its ArgTree:

| arg | meaning |
|---|---|
| `actor` | the branch: `refs/heads/actors/<name>` |
| `request-id` | minted **once per logical request by the caller** and reused on every retry |
| `payload` | the message |

`request-id` doubles as the cache discriminator (two distinct requests never
collide) and the idempotency key (a retry is recognisably the same request).
Do not generate a fresh random nonce per *attempt*: that would defeat both.

No caching switch is needed. Caching a reply is correct exactly when the
request is a replay.

## State layout

```
actor.json        { "schema": 1, "kind": "pure" | "effectful" | "daemon" }
gen               integer; incremented by every commit on the chain
state/            the actor's own data (opaque to caos)
applied/<id>      reply blob for each recently applied request-id (bounded window)
claim             present only while an effect or a daemon is in flight:
                  { "request-id", "owner", "gen", "lease-until" }
```

Every update is **one commit whose parent is the observed head** — a linear
chain. `gen` is the fencing token: strictly increasing along the chain, and
available to external systems that can reject stale tokens.

## Pure actors

```
loop:
  head  = fetch(actor)                      # observed head
  if head.applied[request-id]: return it    # replay: already applied
  (state', reply) = handle(head.state, payload)
  new = commit(parent=head, state', applied+={request-id: reply}, gen+1)
  push --force-with-lease=actor:head  new:actor
    ok        -> return reply
    rejected  -> continue                   # lost the race: re-read and re-apply
    ambiguous -> fetch; if new visible return reply, else continue
```

- The handler is pure, so the worker retries **internally**; the caller never
  sees contention, only latency.
- The crash window (push succeeded, reply never delivered) is closed by the
  `applied/` lookup: the retry finds its own request-id and returns the recorded
  reply without applying it twice.
- `applied/` is a bounded window (oldest pruned in the same commit). A retry
  older than the window is the caller's problem; choose the window to exceed
  any realistic retry horizon.

## Effectful actors

Effects cannot be retried by re-running the handler, so the worker first wins a
**write-ahead claim**:

```
head = fetch(actor)
if head.applied[request-id]: return it
if head.claim and not expired(head.claim):
    if head.claim.request-id == request-id: # our own earlier attempt, see below
    else: fail(retry-later)                 # someone else is mid-effect
claim = commit(parent=head, claim={request-id, owner, gen+1, lease-until})
push --force-with-lease=actor:head claim:actor   # FAILS -> abandon; do NOT run the effect
perform effect (idempotency key = request-id; fencing token = gen)
done  = commit(parent=claim, state', applied+=..., claim removed, gen+1)
push --force-with-lease=actor:claim done:actor
```

- Only one worker can win the claim push for a given head, so only one performs
  the effect. That is the single-writer guarantee for effects.
- The claim carries `lease-until`. A later worker that finds an **expired**
  claim may take it over with a new commit on top; it judges expiry by its own
  clock. Clock skew only affects *when a takeover is attempted*. Safety still
  comes from the CAS chain: a zombie's `done` push fails because the head moved.
- **Crash between claim and done** leaves the tip at "claimed, not done". The
  takeover path must reconcile. Which policy applies is the actor's choice:
  - *at-most-once*: never redo; mark the request failed and surface it;
  - *at-least-once*: redo, relying on the request-id as the external
    idempotency key, or first ask the external system what happened.
- A zombie that already performed the effect cannot be undone by the CAS.
  Where the external system can check it, pass `gen` as a fencing token so it
  rejects the zombie.

## Daemonic actors

A daemonic actor is the same worker, except it stays up:

1. On its first request it takes a **claim commit** as `owner=<container>` with
   a lease, then registers as a runner polling with
   `required: {required-actor: <oid>}`.
2. It serves requests from memory. Every state change is a normal commit
   pushed with the lease. Commit cadence is the actor's choice — per request
   (durable, slower) or batched (fast, bounded loss on crash) — and the lease
   must be renewed (by any commit) before `lease-until`.
3. When its poll returns `idle`, it pushes a **release commit** (state flushed,
   claim removed) and exits. This is the existing ski-rental rule: the poll TTL
   is the idle budget. No new caos op is needed to "persist after an idle
   period"; the release commit is the persist.
4. A second container for the same actor fails its claim push and exits. A
   crashed daemon's claim simply expires, and the next request takes over.

There is no checkpoint operation in caos. The chain *is* the checkpoint log; an
actor that wants stronger durability pushes more often.

### Routing and cold start (needs a decision)

A job with a `required-actor` arg matches **only** that actor's poll; if no
daemon is parked it waits, then fails at the pending deadline. So callers
cannot send such requests blindly. Sketch: callers send a **front request**
without the required arg. A one-shot front worker reads the branch:

- no live claim: it handles the request itself (pure or effectful), or starts
  the daemon and claims on its behalf;
- live claim: it forwards the same request as a sub-run with the
  `required-actor` arg set, then returns the reply.

The forwarding hop costs a worker start per request, which defeats part of the
point of a warm daemon. The alternative is callers that know a daemon is up
(from the actor's own state) and address it directly, falling back to the
front request on failure. See open question 1.

## Use case: the test stack

Today `caos-test` brings a stack up as children of one job and it dies with
that job. As an actor:

- **State** is a small manifest: the stack's address, the image digest it runs,
  a generation. It is *not* the stack's data (Redis, volumes); those are
  rebuilt or left in the persistent volume the runner already mounts.
- **The daemon** is the container running `stack/serve`. It claims the actor,
  publishes its address into `state/`, and serves "run this" requests.
- **Conversations driving the stack** read the address from the branch, then
  talk to the stack over the runner network. Only the coordination (who owns
  the stack, where it is) is in Git.

This decouples the stack's lifetime from any single test job.

## What caos changes

| change | needed for | notes |
|---|---|---|
| **None** in the server | pure, effectful | CAS push, routing and non-caching of failures already exist |
| Bind actor workers to `git-runner` in their `.caos-expr` | all | same as `llm-step`, `run-and-update-ref` |
| An `actor` helper in `worker-common` (fetch, claim, lease, `applied/` window, ambiguous-push handling) | all | so authors do not each reimplement the loop above |
| A worker can register as a runner (poll/result with the runner token the job payload carries) | daemonic | confirm what `caos runner` exposes to a worker; `runner-protocol.md` describes the nesting rule |
| Refresh the `runner-protocol.md` status line | docs | |

## Non-goals

- Server-side ordering, leases or ref policy. The server stays transport.
- A built-in checkpoint or Start message.
- Multi-ref atomic actor updates (possible with `git push --atomic` later).
- Strong isolation between actors (Git transport is unauthenticated today).

## Open questions

1. **Cold start and routing** for daemons (above): front request with a
   forwarding hop, or caller-side addressing? Does forwarding via `/sub-run`
   work when the target is a parked runner?
2. **Lease source.** Workers judge expiry by their own clocks. Is that enough,
   or should the server expose a time/lease primitive? The runner protocol
   already lists lease-based dead-worker detection as future work; actors make
   it more valuable.
3. **History growth.** Every request is a commit and GC is off. Pure actors
   with a hot path will grow the store. Options: periodic squash into a new
   root (resets the chain, which breaks `gen` monotonicity unless it is carried
   over), or a per-actor compaction worker.
4. **`applied/` window sizing**, and what a caller sees for a retry that
   falls outside it.
5. **Authorization.** Do we want per-namespace write control on Git pushes
   before actors hold anything sensitive?
6. **Crash policy default** for effectful actors: at-most-once or
   at-least-once? I would make the actor choose explicitly in `actor.json`.
