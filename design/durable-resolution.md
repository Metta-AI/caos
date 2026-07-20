# Durable, multi-server promise resolution — design note

**Status:** proposed / deferred. The current pipeline (blocking server
threads, in-process single-flight and cycle stacks) is correct and fine for a
single-server prototype; this note is the intended evolution for durability
and horizontal scaling. Decided in a design discussion (2026-07); not yet
built. Pairs with the open items in `map-then.md` ("Durability",
"Concurrent duplicate runs").

## Problem

Promise resolution today blocks **one server OS thread per pending node**:
`dispatch` blocks on a channel until a runner posts; `resolve_promise` fans
map-children out with `std::thread::scope` and the parent blocks joining it.
Workers never block (a worker computes a value or records a continuation and
exits — the load-bearing invariant), but the server does. Consequences:

- a wide/deep DAG holds O(nodes) blocked threads — stacks + scheduler
  pressure bound how much can be in flight;
- **not durable** — promises live in server threads, so a restart loses every
  in-flight resolution;
- **single-server** — the resolution state (which node waits on what, who
  owns an in-flight run, the cycle stacks) is all in process memory, so a
  second server instance can't participate.

## The rework: store the work, don't block on it

The continuation `{in, map?, run?, then?}` is *already* content-addressed
data. So resolution becomes an explicit work queue instead of a call stack:

- a table of **pending nodes**, each with its outstanding-child count and its
  `then` continuation;
- a **completion index** keyed on request hash: when a result arrives, look
  up who was waiting on it, decrement, and enqueue the `then` when the count
  reaches zero;
- a fixed worker pool pulls runnable requests. No thread parked per node.

All of this state lives in **Redis**, which is what makes it both durable
(survives a restart) and multi-server (any instance pulls pending nodes). The
result cache and pinned refs are already in Redis; this extends the same
principle to the in-flight state.

## Single-flight becomes a lease

With no blocked threads, "identical request already running" is not a parked
thread on a channel — it's a **Redis lease**: `SETNX caos:lease:<req>` with a
TTL elects the owner; a duplicate simply **appends its continuation to that
request's waiter list** and does nothing else. When the owner completes it
fans the result out to the waiter list (the same `flights` idea, but the
waiter is a stored continuation, not a `mpsc::Sender`). Lease renewal /
expiry reaps an owner that dies mid-run.

Consequence: the **deadlock** concern disappears entirely — there are no
blocked threads to deadlock, so the in-process `parked` registry and the
`park_would_deadlock` waits-for walk are retired. What remains is **cycle
detection**, which is needed regardless (a cyclic computation has no fixpoint
and must error, not spin).

## Cycle detection: one stack per request + wait-for edges, in Redis

Two structures, both in Redis, chosen to keep **exactly one stack per
request**:

1. **The ancestor stack** — `caos:stack:<req>`, set at **first arrival** and
   never appended to. A request reached again (a diamond's second edge, a
   deduped concurrent hit, a waiter) contributes nothing to it. The run-time
   check is unchanged: computing `req` with ancestor stack `S`, error if
   `req ∈ S`. This catches every cycle that closes **within one descent's
   chain** — all recursion (deep-deps, folds), the overwhelming majority.

   Keeping only the first-arrival stack means a cycle that closes via a
   *later* arrival's path isn't on this request's stack — but it is on the
   stack of some other request on the cycle (the cycle is strongly connected,
   so some node was reached first-arrival-down with the closing node above
   it). Caught a few hops later, never missed.

2. **Wait-for edges** — `caos:waits:<req>` = the set of requests `req` is
   currently waiting on (one entry per real dependency, *not* a path). This
   is the one thing a single upward stack cannot see: the **distributed
   concurrent cycle** — two top-level requests that are each other's
   dependency (`a→b`, `b→a`), deduped at the crossing so neither descent ever
   gets the whole loop onto its stack (both stored stacks are `[]`). A stack
   records *ancestors* (upward); this cycle is only visible across the two
   descents' *downward* reach. So on dedup/park, do a reachability walk over
   the wait-for edges (`a→{b}`, `b→{a}` closes) and error. These are single
   edges — the true graph, stored once — so they do **not** reintroduce
   multiple stacks per request.

The distributed case is rare (two clients concurrently asking for opposite
ends of a mutual dependency); a **timeout reaper** is an acceptable cheaper
alternative to the edge walk if eager detection isn't wanted — wasteful,
never wrong.

## What this retires

- the in-process `flights` / `parked` maps and `park_would_deadlock`
  (`compute.rs`);
- blocking `dispatch` + `thread::scope` fan-out — the thread-per-node ceiling;
- per-frame materialized stacks and per-waiter stack copies (one stack per
  request, in Redis, plus edges).

## Trade-offs / open

- **Detection latency** — a cycle may sit in the queue a few hops (or, for
  the reaper option, a timeout) before it's caught. Bounded by cycle length;
  a cyclic computation was never going to produce a value anyway.
- **Lease liveness** — TTL + renewal must cover the longest legitimate run;
  too short re-runs (wasteful, first-write-wins keeps it correct), too long
  delays reaping a dead owner.
- **The rewrite itself** — turning recursive blocking resolution into an
  explicit state machine with completion-keyed wakeups is real work; do it
  when durability or horizontal scaling is the goal, not before.
- Redis is now on the critical path for resolution (today it's best-effort
  cache only) — a Redis outage stops progress rather than just slowing it.
