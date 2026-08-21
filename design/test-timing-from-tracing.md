# Timing in `run-tool test` from the tracing infra

`caos-cli run-tool test`'s report ends with timing: a `build (Xs)` line, a
`tests (Ys)` line, and a per-test `Ns` column. Every one of those numbers is
manufactured inside the `summarize` stage of `caos-tools/test/worker.sh` and
baked into the `report` blob. That is the wrong place for a duration to live,
and the worker says so:

> A duration is a property of a RUN; a result is a property of its INPUTS.

This document is the plan to stop baking timing into the result and instead
**reconstruct it at print time from the trace record** — the per-ArgTree,
append-only event log the server already keeps (SPEC.md "Tracing",
`crates/server/src/status.rs`).

## What is wrong with the status quo

All three numbers are made in the `summarize` stage and frozen into a
content-addressed value:

1. **`build (Xs)`** — read from `/cas/args/build-time`, curried down from the
   build tool's own `time` file through four stages.
2. **`tests (Ys)`** — `$(date +%s) - start-time`, where `start-time` is a
   `date +%s` stamp the `fanout` stage curries in one line before it fires the
   fan-out. The stamp exists **only** to force the summariser to never cache.
3. **per-test `Ns`** — `/cas/args/children/<t>/seconds`, a duration
   `run-test.sh` writes *into each test's result tree*.

Consequences, all documented as warts in the worker itself:

- **Stale on a cache hit.** Per-test `seconds` is inside a content-addressed
  result, so an unchanged test replays whatever it cost the last time it
  *actually* ran — hence the report's required disclaimer, *"times are each
  test's LAST ACTUAL RUN."* This first bit when a report replayed times
  measured while the engine was unpacking a new image across twenty concurrent
  stacks: it read as "the tests are still slow" when they were not.
- **A timestamp smuggled into args** (`--start-time`) purely to defeat caching
  of the summariser — a hack whose only job is to undo the caching the rest of
  the system works to get.
- **Four stages of plumbing** (`build-time`, `start-time`) threaded through
  `deepener` → `deepen` → `fanout` → `summarize` for numbers none of those
  stages care about.

## What tracing already gives us

`crates/server/src/status.rs` records, per **ArgTree** (the cache key, *not*
the result), an append-only list of events with wall-clock microsecond
timestamps: `requested` / `started` / `ended(ok)` / `child(name, arg_tree,
via)` / `continuation(handler)` / `out-trace`. This is the right source for
every number in the report:

- It is keyed by **run identity**, so a cache hit reads the *original* run's
  real timing rather than a stale baked-in number — exactly the property the
  summariser wishes it had.
- The reuse inference (`child.ended < parent.started` ⇒ reused) is already
  computed, and `GET /status/<hash>?all=1` (the Complete view) keeps finished
  nodes and marks the reused ones.
- The `test` fan-out is fully represented in the trace: the build subtree hangs
  off the `eval`/`run` continuation children, each test is a `map` child of the
  `fanout` node (named by its test name), and the phase boundaries are just
  node spans. Node names already map trace nodes → test names, so no separate
  correlation is needed.

## The load-bearing constraint

SPEC's two-caller contract: the `report` is the tool's whole answer to **both**
`run-tool` and the agent (`worker-llm-step`), and *a tool must not behave
differently depending on who invoked it*. Today the agent side
(`std/llm-step/src/tools.rs::tree_tool_result_block`) prints the `report` blob
verbatim and holds only the **result** hash.

Two things follow:

- Any timing baked into the report is a lie on a cache hit.
- Any timing added **only** by the CLI client makes the agent see less than the
  human — a caller-dependent difference the contract forbids.

So the only correct design reconstructs timing **at print time from the trace**
(keyed by the stable ArgTree), in **shared code both callers call**.

## The plan (option A)

### 1. The worker's report becomes purely structural

`caos-tools/test/worker.sh`'s `summarize` stage stops producing timing. It
drops:

- the per-test `seconds` read and the `Ns` column;
- the `build (Xs)` and `tests (Ys)` lines;
- the `--build-time` and `--start-time` curries (and the `date +%s` stamp), and
  the pass-through of `build-time`/`start-time` through the earlier stages;
- the "times are each test's LAST ACTUAL RUN" / `--test-salt` disclaimer, which
  only exists because the times were stale.

`run-test.sh` stops writing `seconds` into each test result.

What remains in the `report` is genuinely a function of inputs — the pass/fail
mark per test, the record hash, the OK/FAILED banner, and the failure excerpts
— so it caches honestly and carries no duration at all.

### 2. A shared timing-fold helper in the `caos` crate

A new helper takes the run's ArgTree, calls `fetch_status(t, arg_tree,
/*all=*/true)`, walks the returned tree, and folds timing into the printed
report:

- **build phase** = the build subtree's `ended - started`;
- **tests phase** = the span across the `fanout` node's map children,
  `max(child.ended) - min(child.requested)` — which also surfaces queue wait,
  invisible today;
- **per-test** = each map child's `ended - started` (richer than `seconds`:
  `requested`→`started` exposes stack-scheduling wait per test).

The helper renders the same `build (Xs)` / `tests (Ys)` / per-test lines the
worker used to, so the human- and agent-facing output is unchanged in shape —
only its *source* moves from the result to the trace.

Both callers invoke the helper:

- `cli_run_tool` → `report_conventions` (`crates/caos/src/lib.rs`);
- `tree_tool_result_block` on the agent side
  (`std/llm-step/src/tools.rs`).

### 3. Plumbing required

1. **Expose `ended` on `/status`.** The `Node` JSON in `status.rs` currently
   serializes only `requested` and `started` — not `ended` — so durations are
   not derivable from it yet. Add an `ended` field (the `Record` already
   computes it via `ended()`). This is the one server-side change. The Complete
   view keeps finished nodes, so with `ended` present the client can compute
   `ended - started` per node.

2. **Thread the ArgTree to the CLI caller.** `cli_run_tool` does not currently
   hold the ArgTree: `run_request` returns `(kind, result)` only. Either return
   the arg_tree from `run_request` or have `cli_run_tool` `prepare_request` it.
   Minor.

3. **Thread the ArgTree to the agent caller.** `tree_tool_result_block` is
   handed only `result`; it needs the request id too. This is the real cost of
   A — the harness call sites must carry the request id alongside the result.

4. **Degrade gracefully.** Trace records are `allkeys-lru` evictable
   (`stack/serve`), so a late reader may find the record gone → print the
   structural report with no times. That is strictly better than stale times,
   and matches how `/status` already answers "nothing recorded" as a value
   rather than an error.

### 4. Optional richer source: `out-trace`

Workers can leave perf data at `/cas/out-trace`, surfaced on a node's
`out_trace` in `/status`. The build worker already keeps a "phase clock"
(`caos-tools/build/worker.sh`). Once timing is trace-driven, per-phase build
detail could feed the report from there too — out of scope for the first cut,
but the mechanism is already in place.

## Why not the alternatives

- **Have the `summarize` worker read the trace instead of curried files.**
  Lower blast radius conceptually, but it does **not** fix the core bug: the
  report is still a content-addressed result, so its numbers still freeze on a
  cache hit. It also fights the world/nonce boundary (a worker reading the
  outer server's `/status`) and needs the sibling/fanout ArgTrees the
  summariser is never handed.

- **Client-only decoration** (only `run-tool test` appends a trace-derived
  timing block). Smallest change, still needs the `ended` JSON field — but it
  **breaks the two-caller contract**: the agent would see no times. Acceptable
  only as a conscious divergence.

## Order of work

1. Add `ended` to the `/status` `Node` (`crates/server/src/status.rs`).
2. Add the shared timing-fold helper over `fetch_status(..., all=true)` in the
   `caos` crate.
3. Thread the ArgTree to both callers (`cli_run_tool`/`report_conventions` and
   `tree_tool_result_block`).
4. Simplify `caos-tools/test/worker.sh`'s `summarize` (and `run-test.sh`) to a
   purely structural report; remove the `build-time`/`start-time` plumbing.
5. Extend `tests/tracing` (which already exercises this trace shape) to assert
   the reconstructed timing.
