# Actors — persistent state in Git, single writer by compare-and-swap

**Status:** proposal. Nothing here is implemented. Daemons are deliberately set
aside; see [Deferred: daemons](#deferred-daemons).

Builds on [client-owned conversation refs](client-owned-conversation-refs.md)
(the Git protocol workers already use for conversation heads) and follows the
start/finish shape of `std/run-and-update-ref`.

---

## Problem

caos runs pure, cached, hermetic jobs to completion. We also want named things
whose state outlives any one job: small servers, and eventually a test stack
that conversations drive. Today the only way to keep state is to hand it back
through a caller.

## Model

An **actor** is a pure function from `(state, message)` to `(state', reply)`,
with its `state` kept on a Git branch. Cloudflare Durable Objects are the
closest analog. Four rules define it:

1. **No Start message.** Nothing is created or started. The first request for a
   name finds an empty branch and runs against empty state.
2. **State lives on a branch.** Each request names the branch. The head's tree
   is the state.
3. **A lost race fails the request.** Publishing the new state is a
   compare-and-swap on the ref (`--force-with-lease=<ref>:<observed>`). If it
   loses, the request fails and the caller retries. caos already treats failures
   as retryable: they are never cached.
4. **Messages are idempotent.** Applying a message twice has the same effect as
   applying it once. That is the whole duplicate-delivery story: caos does not
   dedupe for the actor and the actor does not dedupe for itself.

Nothing in the server changes. An actor is a std tool plus a convention for the
inner worker.

### Principle: never manifest the whole tree

caos never materializes a whole Git tree, and actors follow that. The state
reaches the inner worker as a **tree oid** that it reads lazily
(`caos get /cas/args/state/foo`), and the new state leaves as an oid it staged
with `caos put`. Neither the wrapper nor the inner ever checks the state out.

## Design

### Request contract

An actor request is a call to the `actor` wrapper tool with these args:

| arg | meaning |
|---|---|
| `actor` | the branch: `refs/heads/actors/<name>` |
| `inner` | the inner actor: any caos worker request template, in any image |
| `nonce` | any value that makes this ArgTree unique, so the outer request is never answered from the cache |
| `message` | the message: a blob or a tree, opaque to the wrapper and passed to the inner unchanged |

`message` is one entry so that a message field can never collide with `state`
or with the wrapper's own args, and so the inner's input and output are
symmetric (`{state, message}` in, `{state, reply}` out).

The nonce is only a cache-buster. Reusing it across retries or minting a new
one per attempt are both correct, because messages are idempotent.

### Branch layout

```
state/      the actor's state: an ordinary tree, opaque to caos
```

The branch tree is `{state}`. The wrapper hands the inner only the `state/`
subtree, so everything else in the tree is the wrapper's. Nothing else is used
today and nothing is reserved; a later feature (claims, counters) can add a
sibling of `state/` without changing the inner's view. Every update is **one
commit whose parent is the observed head**, a linear chain.

### The inner actor

The inner is an ordinary caos worker with **any image**, including a
`docker://` image or a flake. It is a pure function of its args:

| | |
|---|---|
| in | `/cas/args` with two entries, read lazily with `caos get`: `state`, the tree oid of the `state/` subtree (empty for a new actor), and `message` |
| out | `/cas/out`: a tree `{state, reply}`. `state` is the new state tree (staged with `caos put`); `reply` is a blob or tree |

Two choices here differ from a first instinct:

- **New state goes inside `/cas/out`, not beside it.** The result is what caos
  caches, so a separate `/cas/state-out` outside it would be lost on a cache
  hit. (`/cas/out-trace` is the precedent for data that deliberately stays out
  of the cache key; state must not.) A small helper in `worker-common` can
  expose `state-out` as a path to authors.
- **The state is passed as a tree oid, not a commit.** A commit hash includes
  its parent, so every update would change the inner's cache key and nothing
  would ever be shared. A tree oid is content-only: the same state and message
  hit the cache.

Because the inner is a pure function of `(state tree, message)`, **caching it is
correct**. That is why the nonce goes only on the outer request. A retry after
a lost race reaches a different head and so a different inner request, but a
retry after an unrelated failure with the head unchanged reuses the cached
inner result.

An inner that finishes with `state` equal to its input makes **no commit and no
push**. Reads therefore never race with writers, and observe the head at some
moment during the request.

### The wrapper: a start/finish pair

The wrapper has two positions, like `run-and-update-ref`. It holds no container
while the inner runs, and it needs `git` (selected through `git-runner` in its
`.caos-expr`), which the inner does not.

**Start**

1. Read the branch head with an exact-ref, shallow, tree-filtered fetch. Absent
   branch means empty state.
2. Take the `state/` subtree oid from the head.
3. Build the inner request R from `inner`, `state` (the oid) and `message`.
4. Emit `run-request-then R`, carrying the observed head (and the branch name)
   into the callback.

**Finish**, given R's result `{state, reply}` and the observed head:

1. If `state` equals the input state, return `reply`. Nothing to publish.
2. Otherwise build the root tree `{state: <new state oid>}` by oid, with no
   checkout, and a commit on the observed head.
3. Store the commit through the object API, then fetch just that commit into a
   throwaway scratch repository (origin `CAOS_SERVER_URL`).
4. Push with `git push --force-with-lease=<ref>:<observed> <commit>:<ref>`.
   - ok: return `reply`;
   - lease rejected: **fail the request**; the caller retries;
   - ambiguous: fetch the ref again. Treat "my commit is the head" as success,
     "head changed" as a lost race, and "head unchanged" as an infrastructure
     failure.

The window between start and finish is the race window. It is small compared
with the inner's run time, and the compare-and-swap makes it safe.

### Failure, retry and caching

| event | what happens |
|---|---|
| lost race | finish fails; not cached; caller retries; retry reads the new head |
| inner fails | the request fails; not cached |
| crash after the push, before the reply is posted | the job fails; a retry re-applies the message, which is idempotent |
| inner succeeds, finish fails | the inner's result stays cached; a retry on an unchanged head reuses it |
| duplicate concurrent requests | single-flight coalesces identical outer requests; distinct nonces both run and one loses the race |

## Research: what the existing code gives us

### 1. Compare-and-swap on the server's Git transport

- The server delegates smart-HTTP to `git http-backend`
  (`rust/crates/server/src/git.rs`). It does not set
  `receive.denyNonFastForwards`, and does not need to: the expected old value is
  part of the push command, which receive-pack checks under the ref lock, so
  `--force-with-lease=<ref>:<observed>` is a server-side atomic compare-and-swap.
  Fast-forward-ness is **not** enforced by the server; the wrapper keeps the
  chain linear by always committing on the observed head. This matches the
  "transport, not policy" stance of the conversation-refs doc.
- That doc already defines the client protocol used above: a scratch repo whose
  origin is `CAOS_SERVER_URL`, an exact-ref fetch, a lease push, and the
  re-fetch rule after an ambiguous failure.
- Git is opt-in: only workers bound to `git-runner` have `git`.

### 2. Failures are not cached; successes are

In `compute.rs` (`run_dispatch_inner`), only an `Ok` result reaches
`cache_set`; an `Err` is returned uncached. Successes are cached under the
ArgTree hash with no expiry (Redis is best-effort, and the key is namespaced by
`cache_namespace`). I did not find the server retrying a failed job by itself,
so the retry in rule 3 is the **caller's**.

### 3. No credentials needed

The Git paths are unauthenticated: `handle()` in `main.rs` routes them before
anything else, and only `/runner/*` checks a token. A wrapper needs only
`CAOS_SERVER_URL`, which every worker has.

### 4. Existing machinery to reuse

**`std/run-and-update-ref`** is the async worker behind `llm-step`'s
`run_async` and `spawn_agent` tools (bound in `std/llm-step/.caos-expr`, tested
in `tests/run-and-update-ref`). For `run_async` it runs an already-built
request and appends the task's terminal status, with the result oid, to the
conversation ref that started it. For `spawn_agent` it checkpoints the child
conversation's head onto the parent. It has the start/finish structure the
actor wrapper wants (start emits `run-request-then`, finish updates a ref), and
`refs.rs` has the exact-ref fetch and lease-push logic for conversation refs.
The actor wrapper should reuse or factor out that code rather than duplicate
it. I have read only its header and its use of `CAOS_SERVER_URL`, so how cleanly
it separates from conversation semantics is unverified.

**`TreeBuilder`** (`conversation_protocol::v3::tree`) builds trees by oid with
no checkout: `put_oid(path, mode, oid)`, `delete(path)`, `build(store)`.
`llm-step` uses it to seed a child conversation from the parent's tree. The
wrapper can use it to build `{state: <oid>}` the same way.

### Caveats

- Anyone who can reach the server can rewrite an actor branch. Conversation refs
  already accept this; actors inherit it.
- Git advertises every ref on every push and fetch, so the number of actors is a
  soft scaling limit.
- GC is deliberately off, so actor history is never reclaimed.

## Build plan

The wrapper is a new std tool, `std/actor`, laid out like `run-and-update-ref`
(`rustc` factory, `git-runner` as `--output-runner`). It depends on
`worker-common` and the shared ref code from item 4 above.

0. **Spike (verify before building).**
   - Build a root tree and a commit from oids alone, with no checkout
     (`TreeBuilder` builds the tree; `worker-common` has `write_commit` and
     `write_commit_as`, and I have not confirmed they take a tree oid or that
     `TreeBuilder` can write to the store a wrapper has).
   - Confirm the shallow-fetch-then-push path works for a commit built that way.
   - Confirm a start/finish pair can carry the observed head through the
     callback.
1. **Wrapper and a reference inner.** The inner is a small key-value actor
   (`put`, `get`; `put` is idempotent) in a non-runner image, which exercises
   the "any image" claim.
2. **Tests.**
   - concurrent `put`s to one actor, retried on failure, converge with no lost
     update;
   - a forced lost race fails the request and is not cached;
   - a crash after the push followed by a retry reaches the same state;
   - a read makes no commit;
   - **laziness:** a state with many entries and a message touching one of them
     fetches only that entry's objects (assert on the server's object reads);
   - the inner's result is a cache hit when state and message repeat.
3. **Docs.** Refresh the `runner-protocol.md` status line (it still says "not
   yet implemented", but `runner.rs` implements it), and link this doc.

## Non-goals

- Server-side ordering, leases or ref policy. The server stays transport.
- Built-in dedupe, reply caching or exactly-once delivery.
- External effects. The inner must be pure; effectful actors are an optional
  later pattern (a write-ahead claim commit in a sibling of `state/`).
- Strong isolation between actors.

## Deferred: daemons

Daemons are not designed here. The direction I would take when we return to
them: keep **state and liveness** in a pure actor (messages like `claim`,
`renew` and `release` with a lease), run the live process as a **detached
long-running job in the author's own image** whose supervisor only sends caos
requests, and let clients reach it directly at an address recorded in the actor
state. That keeps git and the runner token out of the author's image. The
unverified parts are how a custom image also gets the supervisor, how a detached
job behaves across a server restart, runner-network reachability, and idle
detection.

## Open questions

1. **Caller retry.** The design assumes callers retry a failed request. Which
   layer owns that for conversations and `map-then`, and does it back off under
   contention on a hot actor?
2. **History growth.** Every state change is a commit and GC is off. Options:
   periodic squash into a new root, or a per-actor compaction worker.
3. **Read consistency.** A read observes some head during its run and is not
   linearized against concurrent writes. Is that acceptable, or should a read
   optionally confirm the head at the end?
4. **Large states.** A state change that touches one path rewrites one path's
   spine in the tree. Does the inner have an easy way to build a new state from
   the old by oid, with no checkout? This is the main usability question for
   authors, and the `state-out` helper in `worker-common` is meant to answer it.
5. **Authorization.** Do we want per-namespace write control on Git pushes
   before actors hold anything sensitive?
