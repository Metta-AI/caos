# Promise chains: constant native stack — design note

**Status:** implemented, in `rust/crates/server/src/compute.rs`
(`run_work_request`, `run_hop`, `PendingHop`, `Chain`). The 64 MiB thread stacks
of the stopgap (#370) are dropped with it: request, ticket and map-child threads
are back on Rust's default 2 MiB, which is what the chain test runs against. The
self-restarting `caosd` from #370 stays. See "What was built" at the end for
where the implementation departs from the design below.

Read [map-then.md](map-then.md) first: it defines promises, continuations and
the `then` step this note changes how the server runs.

## Problem

A long agent session can abort the whole server with a stack overflow.

```
thread 'ticket-<hash>' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

This happened twice on caosd-prod (2026-10-28 18:58 UTC and 2026-10-29
22:14 UTC; exit 134). The container died, the unit stayed "active" for hours
(it was a `oneshot`), and every client saw "caos is DEGRADED".

### Cause (measured)

A core from the first crash shows the overflowed thread's 2 MiB stack holding
**157 nested copies** of one cycle, about 13 KB per hop:

```
run_work_request -> run_dispatch -> run_dispatch_inner
  -> resolve_promise -> run_image -> run_work_request -> ...
```

In `rust/crates/server/src/compute.rs`:

- `run_work_request` reads the ArgTree, checks the result cache, runs cycle
  detection against `stack`, claims a single-flight slot, calls `run_dispatch`,
  then publishes the result.
- `run_dispatch_inner` pushes the ArgTree onto `child_stack`, runs the worker on
  a runner, and if the result is `promise <cont>` calls `resolve_promise`. It
  then caches the resolved result under the request's `key`, unless a `catch`
  fired in this hop's middle step.
- `resolve_promise` runs the optional middle step (`map`, `run`, `request` or
  `eval`), then runs the `then` image through `run_image`.
- `run_image` forms the ArgTree and calls `run_work_request` again, passing the
  same, longer, `stack`.

An agent loop is a chain in which each turn's `then` promises again, so the
native stack depth is the number of turns in one ticket. `resolve_promise`'s own
documentation says it: promises "nest arbitrarily", and the thread "is held for
its WHOLE chain, continuations included."

### A second cost (inferred, not measured)

Each hop holds its own `child_stack: Vec<String>` of length k, so a chain of n
hops keeps about n²/2 strings alive (about 64 bytes each): roughly 800 MB at
n = 5,000. The cycle check, `stack.iter().position(..)`, is O(n) per hop, and
`park_would_deadlock` walks the stack as well. Raising the thread stack does not
touch any of this.

## Goal

Native stack depth that does not depend on chain length. A chain of any length
is bounded by memory, and the heap cost per hop is O(1) rather than O(n).

## Design

### An explicit stack of pending hops

The recursion is not a pure tail call: after its child returns, each hop still
caches its result under its own key, finishes its single-flight owner, publishes
the result and closes its trace node. So it cannot become a bare loop. Keep that
work, and move it from the native stack to a `Vec` on the heap.

1. `resolve_promise` stops calling `run_image` for the `then` step. It returns
   either `Done(result, caught)` or `Next { arg_tree, caught }`: the request the
   `then` forms, plus whether this hop's middle step caught a failure.
2. The caller runs a loop. Each iteration is one hop and pushes a record on a
   heap stack: its ArgTree, cache `key`, flight owner, trace node and `caught`.
3. A hop that runs its worker and gets a plain value ends the chain. The loop
   then unwinds the heap stack in reverse. Every pending hop receives the final
   result, because a hop's result is by definition its `then`'s result, and does
   its post-work exactly as the nested version did.
4. Every hop still goes through the full pipeline, because each is a
   memoization, single-flight and tracing unit: cache lookup, cycle check,
   flight claim, `requested` / `started` / `ended`.

### A persistent chain context

Replace `stack: &[String]` with a persistent parent-pointer list (an `Arc` cons
list), together with a `HashSet` of the ArgTrees on the chain. Appending a hop is
O(1), nothing is copied, and the cycle check is a set lookup. The part of the
walk `park_would_deadlock` needs becomes a walk of the list. (Built as segments
rather than a set per hop; see "What was built".)

### Alternative: grow the stack on demand

`stacker::maybe_grow` around `run_work_request` would make depth limited by
memory with a few lines and no semantic change. It leaves the quadratic heap
cost, so it is a stopgap and not this design. I have not checked how `stacker`
behaves on the server's musl target.

## Invariants to preserve

- **Cache semantics.** A hop's final result equals its `then` request's result.
  Each hop caches under its own key unless *its own* middle step caught a
  failure. A sub-run failure is never cached (see the `catch` notes on
  `resolve_promise`).
- **Single-flight.** An identical concurrent request still joins the one that is
  running, and `park_would_deadlock` still sees the true ancestor chain.
- **Cycle detection.** Re-entering an ArgTree already on the chain is an error. A
  tail chain that returns to an earlier hop's ArgTree must fail with the same
  "run cycle detected" listing, in the same order.
- **Trace shape.** `status::requested` / `ended` and
  `status::continuation(config, parent, kind, Some(arg_tree))` produce the same
  tree for `watch` and `status` (SPEC.md, "Tracing").
- **Secrets, salt and pool inheritance** (`inherited_pool_entries`, the secrets
  context) ride the chain unchanged.
- **Failure paths.** An error anywhere in a chain releases every pending flight
  owner. `FlightOwner` broadcasts when its thread unwinds; every record on the
  heap stack must keep that guarantee.

## Test

End to end, per the repository's test policy:

- A worker whose `then` promises to itself N times. N must be well above what a
  64 MiB stack allows (about 13 KB per hop, so around 5,000): use 20,000 or
  more. It passes only if native depth is constant.
- A chain that returns to an earlier hop's ArgTree, expecting the cycle error.

The nearest existing tests are `tests/run-then`, `tests/eval-then`,
`tests/sub-run` and the `llm-step` tests.

## Evidence

The core files and logs of both crashes are on the caosd-prod box under
`/data/caos/crash-20261028T1858` and `/data/caos/crash-20261029T2214`. gdb could
not unwind the overflowed thread. Reading its stack raw (`x/ga` over the thread's
2 MiB mapping) and counting symbols produced the 157-level figure. Only the first
core was analyzed. The second crash's log has the same shape: `resolving promise`
lines, then the abort.

## Non-goals

- Raising the thread stack again as the fix.
- A depth limit that fails long sessions, unless the maintainers ask for one.

## What was built

The design above, with these specifics:

- **The loop is `run_work_request` itself.** Every caller still calls it with
  one ArgTree and gets the chain's result. Inside, `run_hop` does one hop (cache
  lookup, cycle check, flight claim, dispatch, middle step) and returns either
  the hop's outcome or `Continue { next, hop }`: the `then` request
  `resolve_promise` formed and recorded (`status::continuation`), plus a
  `PendingHop` holding the hop's ArgTree, cache key, flight owner and `caught`
  flag. `resolve_promise` returns `Resolved::Next` for a `then` and
  `Resolved::Done` otherwise; it never runs a `then`. When some hop completes,
  `PendingHop::finish` runs for each pending hop innermost first: cache store
  (unless that hop caught), `status::ended`, then `FlightOwner::finish_with`,
  which publishes a top-level result and broadcasts to waiters. Each hop
  receives the outcome the hop after it produced, so a publication failure
  propagates exactly as it did through nested frames. A panic drops the
  `Vec<PendingHop>`, and each owner's `Drop` broadcasts owner loss, as before.
- **Middle steps still nest natively.** A `run`, `map` child, exact `request`
  or `eval` dispatch is a nested `run_work_request`, which runs its own loop.
  Native depth is therefore the nesting of middle steps, which program structure
  bounds, not the number of turns.
- **The chain is segmented, not a cons list with a `HashSet` per hop.** No
  persistent-collection crate is vendored, and a `HashSet` copied per hop is the
  O(n) the design rules out. Each loop owns one `Segment` (the hops it has run,
  in order, plus an index), shared behind an `Arc<Mutex<_>>`. A `Chain` is a
  parent-pointer list of `ChainLink { segment, cutoff, parent }`: the first
  `cutoff` hops of that segment, on top of the parent chain. Hop k's children,
  and hop k+1, get `base.extended(segment, k + 1)`, one small allocation.
  Membership is one hash lookup per link, and the number of links is the native
  nesting depth. `Chain::to_vec` walks everything, and is used only to print a
  cycle. Lock order is `flights`, `parked`, then segments; a segment is
  otherwise locked only by its own loop's append.
- **Tests** are two new stages of `tests/run-then`: `chain.sh`, a worker whose
  `then` is itself with `--n` one lower, run 500 deep (the old pipeline aborted
  the server at about 150 hops on a 2 MiB stack); and `then-cycle.sh`, whose
  third hop is its first hop's request again, expecting the cycle error to name
  both hops. Each hop is a container, so the chain is hundreds, not the 20,000
  the proposal above sized for a 64 MiB stack.
