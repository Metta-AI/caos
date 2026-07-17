# Cargo workers: building and testing caos in caos — design note

**Status:** proposed. Builds on map-then (`map-then.md`), the runner pool
(`runner-pool-and-cloud-builds.md`), and the agent harness
(`agent-harness.md`), which reserved the slot this fills: *"build/test: the
existing caos-native decompositions (rustc, deep-deps), not bash."*

## Problem

Agents editing caos can build and test it through the bash tool, but that
forfeits everything caos is for: no memoization (every `cargo build` starts
cold in a fresh container), no parallelism beyond what one container offers,
and a cache key of "the whole workspace" at best. The right shape is
caos-native: building and testing decomposed into workers, cached per piece,
parallel across pieces — which is also the template for how *any* project
uses caos, not a caos-specific affordance.

Two loops exist today:

- **inner:** the cargo workspace (14 crates, ~176 locked deps, pinned
  toolchain);
- **outer:** nix/crane builds the binaries and images; `tests/run-all.sh`
  drives integration tests against a live stack (`caosd up`).

This note moves the inner loop, and most of the test suite, into workers.
Nix remains the bootstrap and trust anchor (see [Bootstrap](#bootstrap)).

## The cargo worker family

The core contract: **workspace source tree → `{artifacts, diagnostics,
exit}`**. A binary in the result is just a blob; nothing about the contract
says "worker". Compile failures are *values*, not run errors — same contract
as the bash tool — and they cache, correctly, because they're deterministic
functions of the tree.

Keeping "make it runnable" out of the contract is deliberate: most callers
don't want a worker. The agent's `build` tool wants diagnostics; per-crate
jobs want rlibs for their dependents; test jobs want test binaries; the
bootstrap wants `server` and `caos-cli`, which aren't workers at all.
`curry(runner, bin=<blob>)` is one trivial, separable step for the callers
that do — and keeping it out keeps the compile cache shared between "does it
build" and "publish this worker".

### Phase 0 — the toolchain image

One worker image carrying the pinned toolchain plus the workspace's deps,
**vendored and pre-compiled**, keyed on `(rust-toolchain.toml, Cargo.lock)`.
Crane already builds a deps-only artifact derivation; bake it in at nix-build
time and publish via `build-builtins.sh` like any builtin.

The spike (below) settled how this image must be put together — a
**self-contained nix image**, not a thin delta on a stock rust base:

- **Same path.** Cargo fingerprints are absolute-path-keyed: relocating a
  target dir recompiles nearly everything; re-materializing sources at the
  *same* path recompiles nothing (`0/144`). So the deps bake's workspace root
  is recorded in the image and the worker rebuilds the workspace exactly
  there — a fixed path is trivial inside a container.
- **Same toolchain — really the same.** Dep artifacts contain proc-macro
  dylibs and build-script binaries linked against the *compiling* toolchain's
  glibc; a stock rust base's rustc can't load nix-glibc proc macros (two
  glibcs, one process), and a version-matched stock base would be a fragile
  coupling anyway. The pinned nix toolchain bakes and uses its own artifacts.
- **Fresh mtimes, not epoch.** Materializing sources with *fresh* mtimes is
  the safe choice: the deps' vendored sources sit at store-epoch mtimes (and
  stay fingerprint-fresh), while workspace crates — whose baked fingerprints
  came from crane's dummy sources — always rebuild. Epoch-stamping the
  sources could false-validate dummy artifacts; phase 1's semantics
  (workspace always recompiles, deps never do) need no fingerprint surgery.

The cost is a big self-contained image (~800MB compressed) through the git
import — an accepted, documented exception to "big blobs never ride git",
paid once per toolchain/lockfile bump. If it hurts, the fix is a
registry-streamed image build (the flake-worker descendant), not a stock
base.

Initially the image is nix-built, so a `Cargo.lock` change needs a host
rebuild. That's rare and acceptable — and it isn't structural: workers can be
granted network, and `Cargo.lock` checksums pin every byte `cargo vendor`
fetches, so a later `cargo-deps` worker can rebuild the deps layer *as a
cached worker run* (lockfile in the key = the hermeticity; the fetch is just
transport). Determinism comes from pinning, not from a network fence.

### Phase 1 — whole-workspace build and test

`cargo-check` / `cargo-test` workers: materialize the workspace source tree,
run `cargo build` / `cargo test` `--offline` atop the baked deps, return
`{artifacts, diagnostics (rendered + JSON), exit}`.

- **mtimes:** materialized sources get *fresh* mtimes (see phase 0) —
  workspace crates always rebuild, the baked deps never do; the fixed
  container path does the rest.
- **Agent tools:** llm-step grows `build` and `test` — strip `.caos/`,
  run-then the worker, render cargo's JSON diagnostics compactly under the
  transcript budget. Failures come back `is_error`, like bash.

Value delivered immediately: a no-change rebuild is a cache hit, identical
trees across conversations and branches share one compile, and the work runs
off the agent's slot (run-then — no held container). Honest caveat: this
phase materializes the whole *source* tree in one worker — a deliberate,
bounded interim violation of the never-materialize rule (the source is small;
`target/` never rides). Phase 2 removes it.

### Phase 2 — per-crate decomposition

deep-deps is the model — it was effectively designed as this prototype. As
implemented (`worker-cargo/src/decompose.rs`, `mode=all` + internal
`crate`/`job`/`combine` positions):

- **`all`** maps over the workspace members and combines per-member results
  into the flat modes' `{exit, stdout, stderr}` — callers (the agent tools)
  can't tell the difference.
- **`crate`** is cheap orchestration, whole-tree-keyed *on purpose* (it
  re-runs on any edit, compiles nothing): parse manifests, compute the
  member's dep closure, **prune** the workspace to what its build reads —
  root manifest + lockfile + every member manifest + the closure's sources,
  all CAS links — and map-then over the direct deps with itself (`cmd=dep`)
  into a `job`.
- **`job`** is the compile, keyed on (pruned tree, children, name, cmd) —
  the narrow key that buys incrementality. Own sources fresh-mtimed,
  everything else epoch (sound *because* content-addressing guarantees the
  children artifacts were built from exactly these bytes), non-closure
  members stubbed via their declared target paths, children's `target/`
  merged beside the baked crates.io artifacts, then `cargo <cmd> -p X`.
  A dep job's result is `{target: own delta ∪ children's}`, so parents merge
  only direct deps; a **failed dep propagates as a value** — its
  `{exit, stdout, stderr}` becomes the dependent's result uncompiled, so
  diagnostics bubble to the top attributed to the crate that broke.

Edit `worker-rgrep` → recompile one crate. Edit `worker-common` → it plus
dependents, siblings in parallel as map children. Untouched members: cache
hits on their `job` keys.

The workspace DAG has diamonds (`worker-common`), so map-then's deferred
**single-flight** open item got real — implemented in the server
(`compute.rs`): identical concurrent requests share one run; a parked waiter
that times out falls back to running independently, so a cross-thread cycle
degrades to duplicate work + a clean stack-based cycle error, never a hang.

Known cost: the orchestration (`crate`) jobs re-run on every edit — roughly
one container per (member, dep-position) pair, deduped by single-flight.
That's the deliberate deep-deps trade (narrow compile keys bought with cheap
whole-tree-keyed coordination); if the container-spawn tax bites, batching
the deepen positions is the lever.

### rustc re-layered on cargo

`worker-rustc` already *is* a degenerate cargo project (generated manifest,
`src/main.rs`, path dep on vendored worker-common, `cargo build --offline`).
Re-layer it:

- rustc becomes **pure orchestration**: build the project tree in CAS (pure
  linking), run-then into the cargo worker, `then` = curry the compiled
  binary into the runner. Its contract (single `.rs` → runnable worker)
  survives as sugar.
- With no toolchain baked in, rustc itself joins the runner pool as
  `curry(runner, bin=rustc)`; the `rust:1-bookworm`-based rustc image
  retires. One toolchain image in the system.
- worker-common stops being baked into an image: it rides as a
  content-addressed source tree input, and its compiled rlib becomes a
  shared cached artifact instead of being recompiled per rustc run.
- User workers keep their own tiny deps image (std + worker-common's
  lockfile), not caos's — same worker, different curried deps input. That's
  the generality: a project brings a lockfile-keyed toolchain image and gets
  cached, decomposed builds.

## Testing in workers

### What's under test

**The edited stack, not the outer one.** The outer (known-good) stack
contributes only compute and caching: it runs the test job's container and
memoizes the result; nothing inside the tests talks to it. Inside the
container, the test starts the **edited** `server` binary — built from the
agent's tree by the cargo workers, threaded in as a run-then input — plus
`redis-server` (a single binary), and points the edited CLI's
`CAOS_SERVER_URL` at it. Same shape as `tests/run.sh`, with `docker compose
up` replaced by "spawn these binaries". The built binaries' hashes are in the
job's cache key, which is what makes coverage honest: a server edit re-runs
every integration test; a doc edit re-runs none.

### Phase 3 — the process backend

No docker-in-docker is needed for most of the suite, because the server
never launches containers — **runners long-poll it** (pull-based dispatch,
`runner-protocol.md`). Docker enters only at runnerd's exec edge. So the
process backend is a **process-mode runnerd**: the same poll/claim/result
loop, but the "container" is a **chroot slot** (built as implemented —
`CAOS_RUNNER_MODE=process`): each slot carries the setuid `caos` at
`/bin/caos`, the runner-pool trampoline as `/worker`, a `/tmp`, and a
`/dev/null` (a plain file — the one thing a bare chroot lacked that docker
gave for free: Command stdio opens it *before* exec, and its absence
masquerades as "worker not found"). A job runs `caos runner --job=…`
chrooted into its slot — the whole existing lifecycle unchanged, warm
follow-up polling included. Every runnable worker is a `curry(_, bin=…)`;
the server pairs with `CAOS_IMAGE_RESOLVE=none` (images pass through
unconverted — no registry, and no redis either, since the cache is
best-effort). Requires root + `CAP_SYS_CHROOT`: a stock container, even
rootless-podman, no extra grants — validated end to end by
`tests/proc-stack` (inner server + process runnerd + a recursive rgrep fold
through map-then promises, in a stock `debian:stable-slim`).

The uid fence survives: the harness runs as root and `caos runner` drops the
worker child to uid 1000 exactly as in a container, so owner-only `/cas`
placeholders, setuid `caos`, and bash-tool's EACCES/`denied` behavior all
reproduce without namespaces.

**Caos-in-caos (built):** the `testenv` image — the bash script worker plus
git, with `CAOS_WORKER_UID=0` in its config: the per-image containment grant
that lets its jobs run as root, which the inner stack needs (setuid installs
into the slots, chroot). `tests/test-in-caos` runs the whole inner flow —
edited server + process runnerd + a recursive fold — *as a caos worker job*
keyed on (script, built binaries): the first run costs ~17s, the identical
re-run 61ms. That is the tests-as-jobs contract demonstrated on itself; the
remaining work is mechanical — a per-test harness script parameterized over
`tests/<name>`, fixture binaries as args (with a `nix` shim for tests that
build fixtures), a thin process-mode `std` published inside (each entry
`curry(dummy, bin=<binary>)`), and a `test-all` map. Two isolation lessons are
load-bearing, both from the same root cause — a nested stack shares the
outer's ambient environment:

- The runner hands worker scripts the OUTER run's `CAOS_STD`/`CAOS_SALT`; an
  inner client that inherits them builds requests naming a std tree the inner
  server has never seen. Inner harnesses unset both.
- **A nested stack must not share the outer redis.** The worker's container
  sits on the outer `caos-net`, where `caos-redis` resolves — but the result
  cache maps *request-hash → object-hash*, and object presence is per-repo.
  Request hashes are content-addressed, so an inner computation collides with
  an outer one and gets a cache hit pointing at an object that lives only in
  the outer git repo: the pin fails ("nonexistent object"), the fetch fails
  ("not our ref"). Two stacks may share a redis **only if they share the git
  repo**. The inner server points `CAOS_REDIS_ADDR` at a dead port — errors
  read as misses, every result computed in and pinned from the inner repo.
  (Insidious failure mode: the first run *populates* the poison and passes;
  every run after it fails.)

Each `tests/<name>` becomes one job keyed on (server bin, cli bin, std
workers, test tree); a `test-all` worker maps over `tests/` — parallel, and
a test whose inputs didn't change never re-runs. That's also the CI story.
`chat-online` (real API key) stays host-side/self-skipped as today.

There's a pleasing recursive check here: the inner stack running the suite
is caos-under-caos, so "does the edited caos still run workers correctly" is
tested by *using* the edited caos to run them.

### Phase 4 — the podman class

The process backend leaves the docker-facing slices untested inside: the
git-docker → OCI convert, registry push, `docker://` base stacking, and
runnerd's docker half. Running podman *inside a test worker* moves those in
too (with a registry binary in-container for the push tests).

What that actually requires — precisely, because it's less than it sounds:

- There is one kernel. The outer worker's seccomp/AppArmor confinement is
  inherited transitively by everything nested, and Docker's *default*
  profile blocks what container-creation needs (`mount`, `pivot_root`,
  userns creation) — capability-gated against the *outer* container's cap
  set, so in-namespace `CAP_SYS_ADMIN` never enters into it.
- Therefore the podman worker class needs a **relaxed confinement profile**:
  a custom seccomp allowlist (or unconfined) plus AppArmor unconfined —
  flags runnerd passes for designated images. **No `--privileged`, no added
  capabilities, no devices**: kernels ≥ ~5.12 mount native overlayfs inside
  an unprivileged userns (fuse-overlayfs and `/dev/fuse` are the legacy
  path; VFS the universal fallback). Host policy must allow unprivileged
  userns at all (e.g. Ubuntu 24.04 restricts it via AppArmor) — `caosd up`
  should probe.
- It stays a **designated class**, not the default — not because it grants
  privilege but because unprivileged userns + mount are historically the
  richest kernel-escape attack surface, which is exactly why Docker filters
  them by default. Per-worker containment grants, not dogma (same stance as
  network).
- Base images ride in as inputs (OCI layouts, `podman load`ed), pinned by
  digest in the cache key — network may be granted, but the digest in the
  key is what keeps the run deterministic.

### The tiers

- **Process suite** — the everyday gate: light, fast, covers semantics;
  re-runs (from cache, mostly) on every edit.
- **Podman suite** — the docker-path tests in the relaxed class: heavier
  image, memoized like everything else. Cheapest honest policy is run it
  always and let caching absorb it; gate by path if the weight annoys.
- **Host suite (stage 0)** — reduced to "the flake outputs build" and any
  docker-daemon tests deliberately not moved under podman.

## Bootstrap

The classic self-hosting-compiler shape; the host path never goes away — it
becomes the trust anchor.

- **Stage 0 (nix, exists today):** `nix build` + `caosd up` +
  `build-builtins.sh` produce and publish everything from outside caos —
  server image, runner base, cli, std workers, and the toolchain image. A
  fresh machine or CI bootstraps this way, reproducibly, forever.
- **Stage 1 (caos builds caos):** the cargo workers build the workspace.
  The runner pool already closed the hard part of the loop: a worker is
  `curry(runner, bin=blob)`, so a caos-built binary is *directly deployable*
  — caos-built llm-step, bash-tool, rgrep, rustc need no image minting.
  What stays nix-built is only what runs outside workers or *is* an image:
  the server, the cli, the runner base, the toolchain image — and with the
  `cargo-deps` worker, the toolchain image's deps layer moves inside too,
  leaving nix rebuilds for toolchain bumps only.
- **Stage 2 (the fixed point):** run the suite against both the nix-built
  and the caos-built binaries; they should agree. Nearly free given phase 3.

The circularity dissolves once stated precisely: **the running stack is the
old, known-good caos; the workspace under edit is data.** An agent's
modified caos never builds itself — the outer stack builds and tests it,
like building a new compiler with the old one. Promoting a build to *be* the
stack is a separate deploy step.

## Build order

1. Toolchain image (phase 0) — nix-baked deps, published as a builtin.
2. `cargo-check`/`cargo-test` whole-workspace workers + the llm-step
   `build`/`test` tools (phase 1).
3. Fingerprint-portability spike (phase 2 risk retirement; start alongside 2).
4. Per-crate decomposition on the deep-deps model + single-flight (phase 2).
5. rustc re-layered on cargo; retire the rustc image.
6. Process backend (process-mode runnerd) + per-test jobs + `test-all`
   (phase 3).
7. Podman class: confinement profile plumbing in runnerd + the docker-path
   suite (phase 4).
8. `cargo-deps` worker (network-granted vendoring) — retire the host rebuild
   on lockfile changes.

## Spike results (2026-07-17)

Deps built in dir A with vendored sources (cargo 1.96.0), then reused:

| scenario | recompiles (of 144) |
|---|---|
| no-op rebuild in A | 0 |
| A's target+vendor copied to dir B, fresh source copy | **136** |
| A's path, source re-materialized (target+vendor restored) | **0** |

Path relocation kills reuse; same-path reuse is total, even across a full
source re-materialization. Consequences folded into phase 0 above. Note the
same-path/0-recompile result also means phase 2's per-crate artifact passing
is viable via merged target dirs at a fixed path — but 0-recompile-on-
identical-content cuts both ways: stale *workspace* artifacts at epoch mtimes
could false-validate, so per-crate jobs must place only dep artifacts, never
a stale copy of the crate being built.

## Open items

- Phase 2's exact mechanism (merged target dirs at the fixed path looks
  viable per the spike; metadata pipelining and raw rustc stay fallbacks).
- Single-flight on request hash (map-then's open item) — required before the
  per-crate DAG makes diamond recompiles common.
- How stack config designates the relaxed-confinement class (per-image
  grant, like the network grant), and the `caosd up` host probe for
  unprivileged-userns policy.
- Whether the podman suite runs always (cache-absorbed) or path-gated.
- Containers-vs-processes skew: mount layout / env injection differences the
  process backend can't see; the podman and host suites are the backstop.
- GC of toolchain-image versions in the registry (same concern as worker
  images generally).
- ~~The image bakes the worker binary in, so any caos source change re-imports
  all ~100 layers at publish.~~ Fixed in the initial implementation:
  `std/cargo` is `curry(cargo-base, bin=worker-cargo)` — the runner-pool move
  — so the heavy image is keyed on (toolchain, lockfile) only and a worker
  change ships one blob.
