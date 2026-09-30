# Actors — persistent state in Git, single writer by compare-and-swap

**Status:** wrapper (`std/actor`) and a reference key-value inner
(`tests/actor`) implemented; the spike is resolved (see [Spike results](#spike-results)).
`tests/actor` covers the build plan's cases: concurrent writers converging, a
forced lost race that fails uncached and succeeds on retry, an idempotent
re-apply (which is also the crash-after-push retry), a read making no commit,
the inner's lazy view of the state, and the inner's cache hit. Two caveats: a
real crash between push and reply is not injected (a re-applied message is the
same observable), and laziness is checked from inside the inner, not by
counting server object reads. Both are Go programs on `std/go`. Open question 6
(history fetched on every write) is resolved for the write path: the wrapper
moves the branch with a direct receive-pack command and an empty pack instead
of `git push`, so it fetches no history (see the question). Daemons are deliberately set aside; see
[Deferred: daemons](#deferred-daemons).

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
| `state-ref` | the branch holding the actor's state: `refs/heads/actors/<name>` |
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

1. Read the branch head's oid with `git ls-remote`, then fetch **only that
   commit** (`--depth=1 --filter=tree:0`; the server allows filters and
   fetch-by-oid). The commit names its root tree, and the `state/` entry in that
   tree gives the state oid. Absent branch means empty state.
2. Take the `state/` subtree oid from the head.
3. Build the inner request R from `inner`, `state` (the oid) and `message`.
4. Emit `run-request-then R`, carrying the observed head (and the branch name)
   into the callback.

**Finish**, given R's result `{state, reply}` and the observed head:

1. If `state` equals the input state, return `reply`. Nothing to publish.
2. Otherwise build the root tree `{state: <new state oid>}` by oid, with no
   checkout, and a commit on the observed head.
3. Make the commit available to a throwaway scratch repository (origin
   `CAOS_SERVER_URL`) **without downloading the new state tree**. The new state
   exists only on the server, so this is the main technical risk; see the spike.
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
The conversation semantics live in `refs.rs`, but the Git plumbing it uses is
generic and sits in the `conversation-protocol` crate (`git-cli` feature), which
the actor wrapper can depend on directly:

- `GitStore::scratch(name, remote)` makes a bare scratch repo whose `origin` is
  the server. `read_ref` is a cheap `ls-remote`. `push(&[RefUpdate])` pushes
  with `--force-with-lease=<ref>:<expected>` (and `--atomic` for several refs).
  `GitStore` also implements `ObjectStore` (`read_tree`, `write_tree`,
  `write_commit`, ...), so it can write the commit.
- `cas_append` in `refs.rs` is the right ambiguous-push rule, already tested
  with a fake store: after a failed push, re-read the ref; if the candidate is an
  ancestor of the observed head the push succeeded; if the head is unchanged the
  failure is real; otherwise it was a lost race.

**One thing not to reuse as is:** `GitStore::fetch_ref` fetches with no depth
and no filter into a scratch repo that is cleared for every job. For a
conversation ref that is cheap by design (its trees hold gitlinks). For an actor
it would download the whole history and the whole state closure on every
request. The wrapper must use `read_ref` plus a depth-1, `tree:0` fetch of the
head commit instead.

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

The wrapper is a new std tool, `std/actor`: a single Go program run by `std/go`
(Go is the language for new workers), with start and finish as two positions of
one program like `std/run-and-update-ref`. It shells out to the `caos` CLI and,
for the head read, to `git`. The sections above describe the first design, a
Rust wrapper that pushed from a scratch repository; the shipped wrapper replaced
that with the direct receive-pack command described under open question 6.

0. **Spike (verify before building).** An integration test against the test
   stack, using real `git`:
   - **Push a commit whose new state tree exists only on the server.** Stage a
     state tree with `caos put`, build a commit on the observed head that points
     at it (`TreeBuilder` plus `write_commit`), and push it with a lease, without
     ever fetching that tree into the scratch repo. My best guess is a partial
     (promisor) scratch repo, where the missing objects are "promised" and the
     push sends nothing the server lacks; I have not tried it. Fallbacks, both
     worse: fetch the state closure (fine for small state, but it breaks the
     no-manifest rule), or add a small push-by-oid endpoint to the server (which
     breaks "no server change").
   - Confirm the depth-1, `tree:0` head fetch reads the state oid cheaply.
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

### Spike results

Measured against the test stack with real `git`:

- **The promisor guess works, with one addition.** A scratch repo configured as
  a partial clone of the server (`extensions.partialClone=origin`,
  `remote.origin.promisor=true`, filter `tree:0`) can push a commit whose new
  state tree was never downloaded, but only if that tree's *root object* was
  fetched through the filter first. A commit pointing at an object the repo has
  never seen fails in pack-objects (`Could not read <oid>`); one fetched with
  `--filter=tree:0 origin <state-oid>` makes its children "promised" and the
  push goes through. Cost: one tree object per update.
- **Shallow fetches cannot push.** The server answers `shallow pushes are not
  accepted`. Start may read the head with `--depth=1`; finish, which pushes,
  fetches the parent commit without depth (commits only, no trees). That is
  linear in history length, so it sharpens open question 2 (history growth).
- The start/finish pair carries the observed head and input state through the
  callback by currying them onto the wrapper's own ArgTree.

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
6. **Finish fetches every commit on the branch.** The spike showed that a push
   cannot come from a shallow repository, so finish fetches the parent commit
   without `--depth` (`--filter=tree:0`, so commits only, no trees). That
   downloads the actor's whole commit history on every write: cost and latency
   grow linearly with the number of updates, and GC is off, so the history
   never shrinks. Start is unaffected (it reads the head at depth 1). This is
   the same problem as question 2 seen from the write path, and it is the
   reason that question matters now rather than later. Options to investigate:
   - make the parent promised rather than present, by dropping the `shallow`
     file after a depth-1 `tree:0` fetch so the head is the only commit held
     and its parent is absent but listed by a `.promisor` pack. **Tried; it
     does not work with `git push`** (40-commit branch, empty scratch repo):
     the push fails with `Could not read <parent>` / `could not parse commit
     <parent>`, with or without `--no-thin`. The depth-1 pack is marked
     promisor, but the pack-objects that `send-pack` starts walks the head's
     parents to mark them uninteresting and does not tolerate a missing one.
     With the `shallow` file kept, the server refuses the push instead
     (`shallow pushes are not accepted`). Only the full-history fetch
     pushes;
   - **bypass `git push` and speak receive-pack directly. Tried; it works**
     (`tests/actor-ref`). A push is a command `<old> <new> <ref>` plus a pack,
     and the pack may be empty when the server already has the new object. A
     commit made with `caos put-commit` is already on the server, so finish
     POSTs one pkt-line command and an empty pack to
     `$CAOS_SERVER_URL/git-receive-pack`: no scratch repository, no promisor
     setup, no fetch of the parent or of any history. The server does the
     compare-and-swap: a stale `<old>` is answered `ng <ref>` and the ref does
     not move, and the right `<old>` is accepted. The result is an ordinary
     branch that git can fetch. This resolves the history cost of this
     question for the write path, and needs no server change. `std/actor`
     should use it;
   - squash periodically (question 2), which bounds the chain;
   - add a small server-side push-by-oid endpoint, which breaks "no server
     change".
3. **Read consistency.** A read observes some head during its run and is not
   linearized against concurrent writes. Is that acceptable, or should a read
   optionally confirm the head at the end?
4. **Large states.** A state change that touches one path rewrites one path's
   spine in the tree. Does the inner have an easy way to build a new state from
   the old by oid, with no checkout? This is the main usability question for
   authors, and the `state-out` helper in `worker-common` is meant to answer it.
5. **Authorization.** Do we want per-namespace write control on Git pushes
   before actors hold anything sensitive?
