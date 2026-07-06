# Map-then: promises instead of a worker call stack — design note

**Status:** implemented on branch `map-then`, in three commits:

1. `worker_common::map_then` with the target signature, implemented in terms
   of the then-existing blocking `caos run`; fold/deep-deps ported onto it
   (and `pre` dropped everywhere).
2. The implementation moved into `caos run` itself (the worker records a
   continuation; the server resolves it with promises); the blocking form
   removed.
3. `worker-fold` removed: with map-then as the primitive, a worker recurses
   with *itself* (file-count is the model), so a generic fold driver adds
   nothing.

## Problem

A worker that needed sub-computations used to call `caos run` and **block**
until the server had run the child container and returned its result. The
parent container therefore stayed alive — occupying a worker slot — for the
whole lifetime of every descendant. A recursive fold over a tree of depth *d*
holds *d* worker slots at once, so any bound on concurrent workers shallower
than the deepest tree deadlocks: every running worker is blocked waiting for a
child that can never be scheduled. The same chain also forced the server to
dedicate a blocked thread + a live container per tree level, and made a warm
pool (fly machines) impossible to size.

## The change

Worker-side `caos run` is no longer a blocking sub-run. It is a **tail call**
that records a *map-then continuation* as the worker's own result, and the
worker exits. The server resolves the continuation *after* the container is
gone — with **promises** (server-side scheduled sub-runs), not stack frames:

- **`caos run <in> -- [--map=<img>] [--then=<img>]`** (worker form) writes
  `/cas/out` as a **promise placeholder** naming a continuation object;
  `entrypoint` reports `promise <hash>` instead of `blob/tree <hash>`.
- The **continuation** is a content-addressed tree `{in, map?, then?}`: `in`
  is a real tree entry (the data node, mode + oid); `map`/`then` are blobs
  naming images (resolved to hashes / `docker://` refs client-side, so the
  server never sees `/cas` paths).
- The **server**, on a `promise` result, resolves it:
  1. if `map` is given and `in` is a tree: run `map` with `--in=<child>` for
     **each child of `in`, in parallel**; a blob `in` maps to no children.
     The results are assembled into a `children` tree under the original child
     names;
  2. if `then` is given: the request's result is `then(--in=<in>
     [, --children=<children>])` — `children` is passed only when `map` ran;
  3. with no `then`, the result is the `children` tree itself; with no `map`,
     `then(--in=<in>)` is a plain tail call.

  Every sub-run goes through the same internal pipeline (cache → cycle check →
  dispatch → promise resolution), so promises nest arbitrarily: a `map` child
  or a `then` may itself return a promise. The final, fully-resolved
  `"<type> <hash>"` is what gets cached under the original request hash and
  returned to the caller.

`caos-cli run` is **unchanged**: it still blocks at the top level (it holds no
worker slot), and the server resolves all promises before answering.

## Why this cannot deadlock

No worker ever waits for another worker: a container either computes a value
or *describes* the remaining work and exits. The only things that block are
server threads (cheap, one per pending node) — never worker slots. So a global
bound on concurrent containers (`CAOS_MAX_WORKERS`, a semaphore acquired only
for the duration of a single container run and never held while waiting on
anything else) is safe at any setting ≥ 1: some runnable leaf always holds a
slot, finishes, and releases it.

## Expressing the old recursion

There is deliberately no `pre` (a computed set of children) in this version:
a node's own children are what gets mapped. A worker that wants a different
recursion set builds it *locally* (CAS links are cheap and involve no
sub-runs) and points `in` at what it built.

- **a structural fold** is a worker recursing with itself — no generic fold
  driver exists (or is needed). file-count is the model: a tree emits
  `{in, map: file-count, then: file-count}` and exits; invoked with
  `--children` (the `then` position) it combines; a blob is the leaf case. One
  image, three positions, told apart by the arguments present.
- **deep-deps** — no longer built on fold (its `pre` was the point). Its
  `resolve` step was always pure CAS linking, so the worker does it inline:
  `deepen` reads the package's `DEPS`, links the dep subtrees into a local
  tree, and emits `{in: <that tree>, map: curry(self, {mode: deepen,
  packages}), then: curry(self, {mode: finish, pkg})}` — self-recursion
  through `map`. `deepen_all` is a pure map over the package map (no `then`:
  the children tree, keyed by package name, *is* the result).

## Cycle detection

The run stack no longer threads through worker env (`CAOS_RUN_STACK` is gone —
workers never call `/run` anymore). It is an internal argument of the server's
run pipeline: promise sub-runs carry `parent stack + parent request`, and
re-entering a request on the stack fails listing the cycle, exactly as before.
An HTTP `/run` is always top-level (empty stack).

## Parallelism

Map children run concurrently (one thread each, `std::thread::scope`), gated
only by the worker semaphore. `CAOS_MAX_WORKERS` (env, default 8, `0` =
unlimited) bounds concurrent containers across the whole server.

## Open items

- **Concurrent duplicate runs.** Two identical requests in flight both run
  (pre-existing: "no locks yet"). Parallel maps make this more likely — a
  diamond DAG (deep-deps' shared dep) now computes shared nodes once per
  concurrent parent instead of hitting the cache sequentially. Fix is
  single-flight keyed on the request hash; to keep clean cycle *errors* (not
  hangs) it needs a waits-for check before blocking on another thread's
  in-flight run. Deferred.
- **Durability.** Promises live in server threads; a server restart loses
  in-flight resolutions (as it lost in-flight runs before). A journaled
  continuation queue would make them resumable.
- The `serve`/fly dispatch protocol no longer carries `stack`.
