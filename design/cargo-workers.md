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
rootless-podman, no extra grants — validated end to end by the since-retired
`proc-stack` demo (inner server + process runnerd + a recursive rgrep fold
through map-then promises, in a stock `debian:stable-slim`). The process
backend remains in runnerd as the no-socket fallback; the nested test stack
itself is now socket-only (see the 2026-07-21 unification below).

The uid fence survives: the harness runs as root and `caos runner` drops the
worker child to uid 1000 exactly as in a container, so owner-only `/cas`
placeholders, setuid `caos`, and bash-tool's EACCES/`denied` behavior all
reproduce without namespaces.

**Caos-in-caos (built):** the `testenv` image — the bash script worker plus
git, with `CAOS_WORKER_UID=0` in its config: the per-image containment grant
that lets its jobs run as root, which the inner stack needs (setuid installs
into the slots, chroot). The (since-retired) `test-in-caos` demo ran the whole inner flow —
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

**Suite-in-caos (built, since retired):** the `suite-in-caos` demo generalized
the one-worker `test-in-caos` to a multi-worker smoke suite — a file-count fold, the
deep-deps DAG, an rgrep fold — inside one nested process-mode job, keyed on
(script, binaries): first run ~18s, identical re-run 65ms. It introduces
**inner-std publishing without nix**: for each worker binary, `curry(dummy,
bin=<binary>)`, assemble the `{name: curry}` tree with `git mktree`, push it
to the inner `refs/caos/std`. This surfaced a real generality gap: a worker
that self-recurses through `own_image()` — which is the *unwrapped base*, not
the running curry — must **rebind `bin`** at each recursion, or a
`curry(_, bin)` deployment loses the binary on the first sub-run. `rgrep`
already did this; `file-count` and `deep-deps` did not (they only ever ran as
baked images, where the binding is absent and the rebind a no-op). Teaching
them the rebind makes them runner-pool-native as well as image-bakeable —
the correct shape for every worker, and the reason the nix-free process-mode
publish is possible at all. The nix-building and toolchain-image tests
(cargo*, chat*, rust-worker, proc-stack, test-in-caos itself) stay host-side.

**The unification (2026-07-21): one runner, socket-only, sharing via the
cache.** The phase demos (`proc-stack`, `test-in-caos`, `suite-in-caos`,
`socket-in-caos`, the `test-all` meta-test) are deleted; `tests/run-all.sh` is
THE runner and `tests/lib/run-nested.sh` its one inner script. The settled
architecture, from the restart discussion:

- **Each test is one outer caos job** — the cache unit. An unchanged test is a
  ~70ms hit that never starts a stack. Nothing shares a *live* inner stack
  (ambient daemon state a job's key can't see would break hermetic caching);
  the expensive inputs are shared **through the cache** instead — binaries and
  images arrive pre-built, so a cache-miss stack start is process-spawn cheap.
- **The inner stack is socket-only.** No per-test backend choice: the inner
  runnerd delegates every worker to the outer engine as siblings — bin-workers
  as `curry(runner image, bin)`, the pool shape verbatim; image workers (bash)
  directly. The process backend stays in the tree as the no-socket fallback;
  a hybrid per-job dispatch (chroot slots for bin-workers) is deferred unless
  sibling-spawn latency ever matters.
- **Images cross the boundary by image ID** (a content address — a `:latest`
  tag in a cache key would lie), and the sibling images are the flake's own
  runner + bash worker images. The runner ships as a bare delta meant to stack
  on the stock debian base at registry-convert time; run-all reproduces that
  stacking with a two-line local build, until the flake-build worker (phase D)
  makes image production itself a caos job pushing to the caos registry
  (content-addressed, shared — the safe store for image bytes, unlike redis).
- **The host↔caos interface shrinks to**: a running outer stack + pinned stock
  image refs. Everything derived from the edited tree crosses as git content,
  built by caos jobs (binaries via a `std/cargo --cmd=build` job — phase B).
- **One front door.** The suite itself becomes a worker: build the stack
  pieces (nested cargo run against the outer std — the test job keeps *full*
  outer-std access; only the inner stack is scrubbed), fan out per-test jobs
  via map-then, summarize. Humans (`tests/run-all.sh`) and in-caos agents fire
  the identical jobs, so their runs share cache hits.

Folding the bin-worker tests through the socket surfaced a real client bug:
a worker currying its *own image* from `/cas/args/image` (file-count,
deep-deps, rgrep self-recursion) resolved the path to its recorded git hash —
but when the image rides as a `docker://` **blob** (the nested passthrough
case), that hash is the blob's oid, which no engine can run. Fix in
`resolve_run_image`: a CAS *file* whose content is a `docker://` ref resolves
to the ref itself. Outer stacks never hit this (images there are git trees,
where the oid *is* the image) — nesting is what makes the blob case real.

Folded (2026-07-21): **15 of 16 tests run as nested caos jobs** — the
bin-workers (file-count, dirs-only, deep-deps, rgrep), the bash tests
(symlinks, untracked, run-then), the toolchain tests (cargo-check,
cargo-crates, cargo-self, commit, rust-worker — the cargo base image rides by
ID like the others; the inner std carries build-builtins' full shape including
`rustc = curry(runner, bin, cargo, worker_common)`), and the stub tests
(bash-tool, llm-step, chat-offline — helper binaries come from the job's
`bins` tree via `CAOS_BIN_DIR` instead of host nix, and `CAOS_STUB_HOST`
points workers at in-job stubs: siblings share the job's netns, so the stub
lives at 127.0.0.1, not the engine-host alias). The testenv image grew a
private redis — incrementality tests assert real memoization, and the
hermetic-cache answer is a per-job redis that starts empty and dies with the
job, not the poisoned shared one. `run-then` remains the strongest single
check: continuations, nested runs, and cycle detection all resolve through
the inner server driving socket-delegated siblings.

Nested jobs run **unsalted**: their isolation is inherent (a fresh hermetic
stack per job), and the per-run salt would re-key every job every run —
defeating exactly the cross-run memoization the fold exists for.

`chat-online` folds too (so: **all 16, no host batch, nested is the default
for a new tests/<name>/cli.sh**): the API key rides as an ordinary request
arg — it already travels through request args inside `caos chat` itself, so
this adds no new exposure — and same key = same cache key, so the real-API
test re-runs only when the code or the key changes. (An earlier idea here,
salting that one job per run, was wrong: cached-when-unchanged is exactly
the semantics we want for it, like every other test.) Warm full suite: 86s,
every job a cross-run cache hit; the remainder is flake evals — which phase
B/D erase by moving binary and image production into caos jobs.

There's a pleasing recursive check here: the inner stack running the suite
is caos-under-caos, so "does the edited caos still run workers correctly" is
tested by *using* the edited caos to run them.

### Phase 4 — the image class, by socket delegation (not nested podman)

The process backend leaves the docker-facing slices untested inside: the
git-docker → OCI convert, registry push, `docker://` base stacking, and
runnerd's docker half. Those need a test worker that can run *image*-based
sub-workers, not just `bin`-workers through the trampoline.

**The nested-podman plan is dead — a kernel wall, not a config knob.** Running
a container runtime *inside* a test worker is a second level of nesting, and
its container setup must `mount -t sysfs` into a fresh namespace. In a nested
user namespace whose `/sys` is a locked read-only mount, the kernel refuses
that with `VFS: Mount too revealing`. The check is skipped only for a mount in
the *initial* user namespace with real `CAP_SYS_ADMIN`. Single-level rootless
podman never needs `CAP_SYS_ADMIN` (it borrows namespace-scoped privilege),
which is why containers-in-container already work here — but the *second* level
can't escape the revealing check. Verified directly (2026-07-20): rootless
`podman run` succeeds; `unshare --user --net --mount … mount -t sysfs` fails
"too revealing". So nested podman would need the host sandbox itself launched
with `CAP_SYS_ADMIN` + unmasked systempaths — a real host-isolation
relaxation.

**Instead: delegate to the outer engine's socket (Docker-out-of-Docker).** The
test worker doesn't run a runtime; it talks to the *outer* engine through a
bind-mounted API socket, and the sub-workers launch as **siblings** of the
worker — created by the outer engine at the layer that already works. No second
nesting level, no `mount_too_revealing`, **no host change at all**. The trade
is a per-worker grant of the engine socket (root-equivalent over that engine)
plus a little plumbing. This is the same "per-worker containment grant, not
dogma" stance as network and the uid-0 grant.

Mechanics (all proven end to end, 2026-07-20, by a manual spike since retired
along with the demo tests — the living implementation is
`tests/lib/run-nested.sh`):

- **runnerd knobs** (`crates/runnerd`): `CAOS_RUNNER_SOCKET=<host sock>` makes
  the *outer* runnerd bind-mount the engine socket into a granted worker at a
  fixed path (`/run/caos/engine.sock`, advertised as `CAOS_ENGINE_SOCKET`).
  `CAOS_DOCKER_ARGS` injects global flags before `run` so the *inner* runnerd
  runs `podman --remote --url unix://<sock> run …` — delegating to the socket
  rather than a local (nesting) runtime. Both are additive and env-gated; unset
  = today's behavior.
- **Sibling reachability**: the inner runnerd sets
  `CAOS_DOCKER_NETWORK=container:<self>` so each sibling joins the *worker's*
  network namespace (proven: identical `/proc/self/ns/net` inode). The inner
  server, a process inside the worker on `127.0.0.1`, is then reachable by the
  siblings exactly as `CAOS_SERVER_URL=http://127.0.0.1` — the same URL the
  runner already expects.
- **Client must be remote**: the local `docker`/`podman` here is a shim that
  runs *locally* (i.e. nests). Delegation needs a client that talks to the
  engine's API socket: either the moby `docker` client against the podman
  docker-compat socket (`/var/run/docker.sock` — exactly what the outer runnerd
  already uses, `DOCKER_HOST=unix://…`, no extra flag), or `podman --remote
  --url` against a podman API service. The in-suite path uses the former (the
  socket is already there); the manual spike used the latter.
- **Image resolution**: `resolve_image` passes `docker://<ref>` through before
  any convert, so the first cut references a self-contained runner image the
  outer store already has — no inner registry needed. (The convert/registry
  path — for the git-docker → OCI tests — layers on top later, pushing to a
  registry the siblings can pull, same as the outer stack.)
- The runner image the siblings run must carry a **setuid-root** `/bin/caos`
  (the worker drops to uid 1000 and reaches the root-owned `/cas`, xattrs and
  all, only through it) and the `/worker` trampoline — exactly what the normal
  worker images install. Static musl binaries mean it can be a thin `FROM
  scratch`/debian image.

**Landed in the suite (2026-07-20, now the only nested backend):** the testenv
image carries the slimmed moby `docker` client; the compose runnerd sets
`CAOS_RUNNER_SOCKET=/var/run/docker.sock` (the socket it already has), so every
worker gets it bind-mounted at `/run/caos/engine.sock` (coarse for now — a
per-image grant is future work); `run-nested.sh` runs the inner server + a
docker-mode inner runnerd that launches siblings via the socket. ~20s cold per
test job, **~70 ms cache hit** (identical inputs never re-run) — image-based
workers nesting as caos jobs, the process backend's gap closed.

Still open: refine the socket grant from pool-wide to per-image; the
convert/registry path for the git-docker → OCI tests (siblings pull converted
images from a registry, same as the outer stack) rather than the `docker://`
self-contained shortcut. Base images still ride in as inputs pinned by digest
in the cache key.

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

- **flake-build worker (the third build leg; deferred, user-requested).** A
  stock `nixos/nix`-based worker taking `(flake-ref, attr, curried
  push-creds)`, running `nix build .#attr` then streaming the result to a
  registry (`streamLayeredImage | skopeo`); `/cas/out` is the docker digest,
  memoized on (flake, lock, attr) so repeats never invoke nix. This is the
  runner-pool note's deferred flake-worker. It is the one build a normal
  worker structurally *cannot* do — **images** — so it is how the bootstrap
  finally closes: `build-builtins.sh` becomes caos jobs, nix-on-host shrinks
  to building the single nix-worker image, and a `Cargo.lock` bump rebuilds
  the toolchain image in-caos instead of on the host. Complements, does *not*
  replace, the per-crate cargo path (which stays the fine-grained inner
  loop). Caveats: a cold `/nix/store` needs a substituter (network — fine per
  the worker-network stance); the flake.lock must be pinned for the memo to
  be sound; push creds are curried. The root grant + process backend built in
  phase 3 (the testenv worker) already provide what it needs. Smallest first
  step: build one existing image attr, stream it to the dev registry, prove a
  second run is a caos cache hit with nix never invoked.
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
