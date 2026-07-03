# Isolate-class workers: wasmtime spike results

Status: working end-to-end locally (2026-07-03). fold-wasm is published to std
alongside container fold; all existing tests pass; the docker path is untouched.

## The problem

Fold over an N-node tree issues ~2N `/run` calls, serially, each paying a full
container lifecycle. Measured on the OrbStack dev VM (docker backend,
`tests/fold-bench`, 160 nodes / 120 files, every subtree unique):

| fixture | container fold cold | fold-wasm cold | warm (memoized) |
|---|---|---|---|
| synthetic 160 nodes / 120 files, depth 4 | 61.2s (quiet) – 106.7s (loaded) | **10.9s** (same loaded machine) | 4–5ms |
| real tree: this repo's `crates/` (60 nodes), via `caos-cli` | 37.4s | **10.3s** | — |

(The salt isolates the cold runs from each other; deeper/wider trees widen the
gap, since fold-wasm's wall-clock is visitor-bound while container fold's is
node-count-bound and serial.)

Per-run tracing (`caos-trace` stderr lines, `tests/trace-report.py`): container
runs are p50 ~324ms with effectively **100% in dispatch** — cache lookup and
image resolve are free; it's all `docker run`. The isolate host's own numbers
(`isolate-trace`): **instantiation 3–26µs**, module compile 0ms after the first
job (compiled-module cache). A fold frame is ~10⁵× cheaper than a container.

fold-wasm's remaining 10.9s is entirely its ~121 *visitor* runs (file-count is
still a container image, ~550ms each on the loaded VM, fanned out 16-way per
frame). The orchestration itself — 40 concurrent fold frames — costs microseconds.
The 24-level-deep fixture completes trivially (`tests/fold-wasm`): parent frames
suspend inside `run_many` holding no container, no thread, no slot, so recursion
depth is unbounded — the deadlock class the warm serve pool has is impossible
here by construction.

## What was built

- **Runtime-object mechanism, general from day one.** A worker may be a git tree
  `{".caos-runtime": <blob: host image ref>, "module": <blob>}`. The server
  (compute.rs `runtime_node`/`dispatch_runtime`) detects the marker before
  `resolve_image` and POSTs the job to a warm host container
  (`caos-isolate-host-{key}`, one per host image, serves every module of its
  runtime, no SLOT). Memoization, cycle detection, and result pinning are shared
  with container workers — the request format `{image, args, std, salt}` is
  unchanged, so an isolate worker is just a different image shape. The marker
  carries the **host image hash**, so the cache key covers the runtime version.
- **isolate-host** (crate + musl-static service image): embeds wasmtime 46
  (pooling allocator), exposes the `caos_abi_v1` guest ABI — `job`, `tree`,
  `get`, `put_blob`, `put_tree`, `run`, `run_many` (host fans nested runs out on
  OS threads, 16-way, order-preserving), `out`, `log` — as two wasm imports
  (`call`/`read`, JSON payloads). Deterministic WASI stubs (clock=0, fixed
  random, no fs/net/env): the ABI is the guest's entire world, so isolate
  workers are *more* hermetic than containers. Tree encoding is byte-identical
  to the client's (unit test pins it against `git write-tree`).
- **isolate-common + fold-wasm** (wasm32-wasip1, 140KB module): fold ported to
  the ABI. Children fold via one `run_many` batch; blob children with no `pre`
  short-circuit straight to the `post` request with the canonical empty
  children-tree — byte-identical to the request the recursive path would
  produce, so leaf results alias across fold implementations (and with
  container fold, given same std/salt).
- **Publishing:** `build-builtins.sh` imports the host image as std entry
  `isolate-host` (which pins its objects — the marker blob only *names* it) and
  assembles `fold-wasm` as a runtime node. `nix build .#fold-wasm` produces the
  module; the toolchain now carries `wasm32-wasip1`.

## Semantics notes

- A symlink leaf counts like any blob leaf (placeholder-file mechanics), in both
  implementations — `tests/fold-wasm` pins parity at 31 on the wide fixture.
  Args-tree entries deliberately drop symlink modes to match the container
  client's arg builder byte-for-byte.
- Divergence to settle: container fold *follows* a symlink-to-dir (recurses);
  fold-wasm sees a blob and treats it as a leaf. Not exercised by std fixtures;
  arguably container fold's behavior (double-counting the target) is the bug.

## Open questions for the team

1. Runner/bash workers as runtime nodes too? The `{host, payload}` shape unifies
   them (`.caos-runtime` = runner image, module = binary) — one dispatch path.
2. Visitor cost now dominates. Options: wasm visitors (file-count is ~20 lines of
   guest code — would collapse the 10.9s to ~ms), the elastic container pool
   (designed, unbuilt), or both.
3. `run_many` concurrency (16/frame) — right default? Env-tunable like
   `CAOS_FOLD_PARALLELISM` would mirror the container-fold design's cache-key
   hygiene (env, never args).
4. Fuel/epoch limits for runaway modules; ABI versioning policy beyond the
   `caos_abi_v1` namespace; TS/V8 as a second host (glibc host image is fine —
   the mechanism doesn't care); Fly machine for the isolate host.
5. Idle/stale host containers: a new host image version strands the old
   container (observed in dev: `caos-isolate-host-<oldhash>` keeps running).
   Needs the reaping story the serve pool also lacks.
