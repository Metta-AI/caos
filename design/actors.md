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
Cloudflare Durable Objects are the closest analogue. Four rules define it:

1. **No Start message.** The container comes alive on the first request and
   exits after an idle period. So every request names the actor, and the
   worker finds the state itself.
2. **State lives on a branch.** The request carries the branch name. The worker
   reads the branch, handles the request, and pushes the new head.
3. **A lost race fails the request.** A push is a compare-and-swap on the ref
   (`--force-with-lease=<ref>:<observed>`). If it loses, the request fails and
   the caller retries. No retry loop inside the actor.
4. **Messages are idempotent.** Applying a message twice has the same effect as
   applying it once. That is the whole duplicate-delivery story: caos does not
   dedupe for the actor, and the actor does not dedupe for itself.

Nothing in the server changes for this. An actor is a convention for workers
(see [What caos changes](#what-caos-changes)).

| flavor | process | when |
|---|---|---|
| **pure** | one-shot worker per request | state is data; the rules above are all it needs |
| **daemonic** | container that registers as a runner and stays up until idle | performance, or a live process (a test stack) |

An actor whose messages cannot be made idempotent, or whose handler has
external effects, layers a pattern from [Patterns](#patterns) on top. None is
built in.

## Research: what the existing code already gives us

### 1. Compare-and-swap on the server's Git transport — supported, in use

- The server delegates smart-HTTP to `git http-backend`
  (`rust/crates/server/src/git.rs`). It does **not** set
  `receive.denyNonFastForwards`, and does not need to. The expected-old-value
  check comes from the push command, which receive-pack applies under the ref
  lock, so `--force-with-lease=<ref>:<observed>` is a server-side atomic CAS.
  **Fast-forward-ness is not enforced by the server**; the actor keeps a linear
  chain by always committing on the head it observed. This matches
  [client-owned conversation refs](client-owned-conversation-refs.md): "the
  server provides transport, not policy."
- That design already specifies the client protocol: a scratch repo whose
  `origin` is `CAOS_SERVER_URL`; read with an exact-ref fetch; append with
  `git push --force-with-lease=<ref>:<observed> <new>:<ref>`; after an
  ambiguous failure, **fetch again** and treat "my object is visible" as
  success, "head changed" as a lost race, and "head unchanged" as an
  infrastructure failure. Actors reuse this verbatim.
- Git is opt-in: only workers bound to the `git-runner` image have `git`.
  Actor workers select it in their `.caos-expr`.

**Caveats.**

- The Git paths are **unauthenticated**. `handle()` in `main.rs` routes them
  before anything else, and only `/runner/*` checks a token. Any worker can
  rewrite an actor branch. We accept that today for conversation refs; actors
  inherit it.
- Git advertises every ref on every push and fetch. Actor refs are permanent,
  so the number of actors is a (soft) scaling limit.
- GC is deliberately off, so actor history is never reclaimed. See
  [Open questions](#open-questions).

### 2. Failures are not cached; successes are

In `compute.rs` (`run_dispatch_inner`), only an `Ok` result reaches
`cache_set`. An `Err` is returned uncached, so **re-requesting a failed
request re-runs it.** That is what makes "lose the race, fail, retry" work.

A successful result is cached under the ArgTree hash with no expiry (Redis is
best-effort). That matters for one reason: **an identical request would be
answered from the cache without reaching the actor**, so a read-style message
would return a stale reply. Every request therefore carries a `nonce` to keep
the ArgTree unique (see the contract below).

I did not find the server retrying a failed job by itself. The retry in rule 3
is the **caller's**, and this design assumes callers retry failed requests.

### 3. Credentials for the branch — none needed in-stack

Because Git transport is unauthenticated, an actor worker needs only
`CAOS_SERVER_URL`, which every worker already has. Credentials are needed only
for *external* remotes or services, and those use the existing secret store
(SPEC.md "secrets"), which injects out of band and never enters the cache key.

### 4. Runner routing — already symmetric

`runner.rs::matches` is pure oid equality in both directions. A job entry whose
name starts with `REQUIRED_ARG_PREFIX` must equal the runner's entry of the
same name, and every entry a runner requires must equal the job's. A job
carrying `required-actor=<oid>` therefore reaches only a runner polling with
`required: {required-actor: <oid>}`, and never leaks to the generic pool. This
is the routing half of daemonic actors and needs no server change. (The header
of `runner-protocol.md` still says "not yet implemented"; the server side is in
`runner.rs`.)

## Request contract

| arg | meaning |
|---|---|
| `actor` | the branch: `refs/heads/actors/<name>` |
| `nonce` | any value that makes this ArgTree unique, so the cache does not answer it |
| `payload` | the message; **must be idempotent** |

Because messages are idempotent, the nonce is just a cache-buster. Reusing it
across retries or minting a fresh one per attempt are both correct.

## Pure actors

```
head = fetch(actor)                        # observed head
(state', reply) = handle(head.state, payload)
new = commit(parent=head, state')
push --force-with-lease=actor:head  new:actor
  ok        -> return reply
  rejected  -> fail the request            # lost the race; the caller retries
  ambiguous -> fetch; if new visible return reply, else fail
```

- The branch tree is the actor's own data. caos reserves nothing in it.
- A duplicate delivery re-applies an idempotent message. A crash after the push
  but before the reply is posted is the same thing: the retry re-applies and
  reaches the same state.
- If the handler reads state that another request just changed, the loser fails
  and retries against the new head. That is the entire concurrency story.

## Daemonic actors

A daemonic actor is the same worker, except it stays up, so it needs a **lock**
to keep a second container from also serving. The lock is a commit:

1. On its first request it pushes a **claim commit** (with lease) that records
   `owner=<container>` and `lease-until`, then registers as a runner polling
   with `required: {required-actor: <oid>}`. If the claim push loses, it exits.
2. It serves requests from memory. Every state change is a normal commit pushed
   with the lease, and any commit renews the claim before `lease-until`. Commit
   cadence is the actor's choice: per request (durable, slower) or batched
   (fast, bounded loss on crash).
3. When its poll returns `idle`, it pushes a **release commit** (state flushed,
   claim removed) and exits. This is the existing ski-rental rule: the poll TTL
   is the idle budget. The release commit is the persist; caos needs no
   checkpoint operation.
4. A crashed daemon's claim simply expires, and a later request takes over by
   pushing a new claim on top. A zombie's next push then fails because the head
   moved.

The claim lives in one reserved file, `.actor/claim`
(`{"owner", "lease-until"}`), so pure actors never see it.

### Routing and cold start (needs a decision)

A job with a `required-actor` arg matches **only** that actor's poll; if no
daemon is parked it waits, then fails at the pending deadline. So callers
cannot send such requests blindly. Sketch: callers send a **front request**
without the required arg. A one-shot front worker reads the branch:

- no live claim: it handles the request itself as a pure actor, or starts the
  daemon and claims on its behalf;
- live claim: it forwards the same request as a sub-run with the
  `required-actor` arg set, then returns the reply.

The forwarding hop costs a worker start per request, which defeats part of the
point of a warm daemon. The alternative is callers that know a daemon is up
(from the actor's own state) and address it directly, falling back to the front
request on failure. See open question 1.

## Patterns

These are conventions an actor may adopt. caos provides none of them.

**Dedupe by request ID.** For a message that is not naturally idempotent, put
the request ID in each commit message and have the worker scan the last few
commits for its own ID before applying. A retry that finds it returns
"already applied". This closes the crash window between push and reply without
a stored reply cache.

**Write-ahead claim for external effects.** If the handler acts on the outside
world, push a claim commit (request ID, owner, lease) **first** and abandon if
the push loses, perform the effect, then push a done commit on top of the claim.
Only one worker can win the claim for a given head, so only one performs the
effect. After a crash the tip is "claimed, not done", and a later worker may
take over an expired claim and either redo the effect (at-least-once, with the
request ID as the external idempotency key) or surface it as failed
(at-most-once). Keep a monotonic counter in the chain and pass it as a fencing
token where the external system can check it, since the CAS cannot undo an
effect a zombie already performed.

## Use case: the test stack

Today `caos-test` brings a stack up as children of one job and it dies with
that job. As a daemonic actor:

- **State** is a small manifest: the stack's address and the image digest it
  runs. It is *not* the stack's data (Redis, volumes); those are rebuilt or
  left in the persistent volume the runner already mounts.
- **The daemon** is the container running `stack/serve`. It claims the actor,
  publishes its address into the branch, and serves "run this" requests.
- **Conversations driving the stack** read the address from the branch, then
  talk to the stack over the runner network. Only the coordination (who owns
  the stack, where it is) is in Git.

This decouples the stack's lifetime from any single test job.

## What caos changes

| change | needed for | notes |
|---|---|---|
| **None** in the server | pure | CAS push, routing and non-caching of failures already exist |
| Bind actor workers to `git-runner` in their `.caos-expr` | all | same as `llm-step`, `run-and-update-ref` |
| A small `actor` helper in `worker-common` (fetch, CAS push, ambiguous-push handling, claim/lease for daemons) | all | so authors do not each reimplement it |
| A worker can register as a runner (poll/result with the runner token the job payload carries) | daemonic | confirm what `caos runner` exposes to a worker; `runner-protocol.md` describes the nesting rule |
| Refresh the `runner-protocol.md` status line | docs | |

## Non-goals

- Server-side ordering, leases or ref policy. The server stays transport.
- A built-in checkpoint or Start message.
- Built-in dedupe, reply caching or exactly-once delivery.
- Strong isolation between actors (Git transport is unauthenticated today).

## Open questions

1. **Cold start and routing** for daemons (above): front request with a
   forwarding hop, or caller-side addressing? Does forwarding via `/sub-run`
   work when the target is a parked runner?
2. **Lease source.** Workers judge expiry by their own clocks. Clock skew only
   affects *when a takeover is attempted*; safety comes from the CAS chain. Is
   that enough, or should the server expose a time/lease primitive? The runner
   protocol already lists lease-based dead-worker detection as future work.
3. **History growth.** Every state change is a commit and GC is off. A hot actor
   will grow the store. Options: periodic squash into a new root, or a
   per-actor compaction worker.
4. **Caller retry.** The design assumes callers retry a failed request. Which
   layer owns that for conversations and `map-then`, and does it back off under
   contention on a hot actor?
5. **Authorization.** Do we want per-namespace write control on Git pushes
   before actors hold anything sensitive?
