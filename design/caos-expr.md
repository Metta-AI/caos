# `.caos-expr`: evaluable trees and DEEP-DEPS resolution (simplified)

Caos provides a standard way for trees to express the computation required build tools or otherwise work with the tree

Examples:
- Tools can be defined in any language and can describe how they should be built
- The whole tree can be restructured so that dependencies are inside the packages that depend on them, supporting argument-addressable compilation

## Evaluating trees

- `caos eval-path [--tree=oid] <path>` interprets `.caos-expr` files that are embedded in the tree from the root to the provided path and returns the result
- Each expression is evaluated in the tree returned by the parent expression. Most expressions will evaluate to a tree with a similar shape to the original. But this is not required. A valid path is one where each segment after an expression is valid in the result of that expression. This can't be determined statically
- A `.caos-expr` is a sequence of lines. Blank lines and `#` comments are ignored. Any line but the last binds a variable; the last line is the file's value:
  ```
    run   <image> -- [--name=value | --name:@=path | --name:commit=rev]
    curry <image> -- [--name=value | --name:@=path]
    # bind a variable (uppercase name), then use it with $NAME:
    FOO=run <cargo-ref> -- --src:@=src
    curry <runner-ref> -- --worker1=$FOO
  ```
  Variable names are `[A-Z][A-Z0-9_]*`; the verbs are lowercase, so a line is an assignment iff it starts `NAME=run`/`NAME=curry`. A `$NAME` in an image position is the object that variable produced; `--k=$NAME` binds that object by reference (at its own kind); `--k=value` is a literal blob. (This replaces an earlier `$( ... )` command-substitution sketch — the variable form is easier to write, read and parse.)
- A `run` expression evaluates to the run's result; a `curry` expression to the curried ArgTree. In practice we dig into `run` results, not through `curry`.

- Arguments are parsed as with a normal curry/run-then command, except that paths are relative to the directory containing the `.caos-expr` file. `/std/...` is interpreted as normal for now (until we remove it later)
- There is no lazy evaluation here
- `eval-path` converts the expression into an arg tree and then requests that the arg tree is run, providing normal caching

Coas use `eval-path` in several places:
- To find tools to register with an agent: `eval-path caos-tools`
- When an agent requests a tool: `eval-path caos-tools/<tool>` to generate the tool
- When running an image: `eval-path <image>`

This replaces many other mechanisms:
- A simple script can curry itself with the bash worker to become a worker
- A rust program can be compiled by rustc, without any help from build-builtins
- A flake can be passed to flake-builder, without any special support for flakes in caos

## Deep deps

Most repos will have a top-level `.caos-expr` that invokes `std/deep-deps` on the tree, allowing any directory to declare deps outside its subtree

## Initial workers

Problem:
- The flake builder can't be built until there's a flake builder. Other workers like rustc could be build by the flake builder but can be built faster if we reuse build results from building the caos core
- Many workers depend on other core workers but still need to be built before the rest of std can be built normally. deep-deps is one such exampe

Solution:
- Instead, we have a seed runner that registers with the caos server for work with an arg tree that exactly matches the arg tree for each image that we seed from the core
- These seeded images can still be described in /std/*, with a `.caos-expr`. They list their image as `std/flake-builder` or whatever else would be appropriate, if everything else existed. And they list their DEPS like normal too
- However, `build-builtins.sh` builds these core images in advance, including transforming them to give them deep deps. The script will have a hard-coded sequence that it moves through to build each image after any needed deps are built
- A core-seeder-runner registers with the server to handle an arg tree that exactly matches the arg tree for each of these core images. Because it is a very specific match for each image, it runs in preference to the regular runner. (Also, much of this happens when there is no regular runner.) core-seed-runner compute the arg trees to seed by using caos to form the arg tree for each of the core images in std. This ensures that it seeds the keys that later code uses

## Removal of `std`

Later we will devise a way to expose `std` in these `DEPS` files so that a subtree may depend on any item in std that it likes. This is better than other mechanisms that we have considered:
- Passing all of std to every worker means that any change to std invalides all cache entries
- Having workers pass std members when they call other workers means that when a worker needs a new std member, it all of its transitive callers must be updated

## Implementation plan (Phase 3: bootstrap + seeder)

Status so far (landed, suite green):
- `caos eval-path` and the `.caos-expr` grammar (Phase 1); whole-tree `deep-deps` published as `std/deep-deps` (Phase 2).
- `resolve_std_image` resolves a `/cas/std/<name>` builtin by running the *same*
  `eval-path` walker over the std tree (`eval::eval_std_entry`): a std-root
  `.caos-expr` is applied first, then the named entry's own — so a std entry can
  be a direct image OR source + a `.caos-expr`, resolved uniformly.
- `std/bash`, `std/merge`: source + `.caos-expr` (`run /std/flake-builder -- --in:@=.`).
- `/std/rustc` gained a **directory interface** (`--src:@=<project dir>` using the
  tool's own `Cargo.toml`) and builds at the cargo image's default profile
  (dev + musl) so a tool's crates.io deps are **reused from the seeded
  `/std/cargo` bake** rather than recompiled.
- `std/rgrep`: `{ src/main.rs, Cargo.toml, .caos-expr }` — a normal tool built by
  `/std/rustc`; `crates/worker-rgrep` deleted (the harness resolves `/cas/std/rgrep`
  like production).

### The core idea

`std/*` is uniform: **every** entry carries a `.caos-expr` describing how it is
built, and callers never special-case anything — they resolve, which forms an
arg-tree and dispatches. The irreducible core is *not* actually built by its
expression; `bootstrap.sh` (the renamed `build-builtins.sh`) hand-builds each
core artifact to be **byte-identical to what its expression would have
produced**, and the seeder injects those artifacts under **the key the
expression evaluates to** — so seeded keys are provably the keys callers hit.

### `bootstrap.sh` (rename of `build-builtins.sh`)

- Hand-builds the irreducible core **in dependency order**, each item matching
  what its `.caos-expr` describes. Mostly this is *currying host-nix artifacts*,
  not running workers: the worker binaries (`rustc`, `deep-deps`, cargo's image)
  are already host-built; "build manually" = curry them onto `runner` / stream
  the image, as today.
- If an entry declares deep-deps, bootstrap **fills the deepened deps itself**
  (hand-deepening), so the tree it seeds matches what the real `deep-deps`
  worker would emit. GUARDRAIL: a test diffs bootstrap's hand-deepened tree
  against the `deep-deps` worker's output on the same input — they must be
  byte-identical, or the seeded key won't match the caller's.
- Emits a **seed record** per core item: `{ required: <the arg-tree's top-level
  name→oid entries>, result: <the hand-built delta/curry hash> }`, published
  under a ref (e.g. `refs/caos/seed`). The `required` set is formed by
  evaluating the item's `.caos-expr` through caos — the same code the caller
  uses — so the key matches by construction, not by a hand-maintained table.
- **No runner, one path.** All seeding is done without a generic runner, so the
  host (`up`/caosd), the nix seed derivation, and the test stack all seed
  identically. (Starting a runner mid-bootstrap is a possible *optimization*
  later, not part of the contract.) Non-core leaf tools (`bash`, `merge`,
  `rgrep`) are not seeded; they build at first use in the live stack.

### `core-seeder-runner` (new)

- A runner (uses the existing `/runner/poll` protocol — **no server change**).
  Reads the seed records and registers one poll per record with `required` =
  that record's arg-tree entries; on a match it **posts the pre-built result
  directly**, spawning no container.
- Wins over `runnerd` automatically: the server already prefers the most-specific
  `required` match (`offer_job` / `best_pending` `max_by_key(required.len())`),
  and a full arg-tree match beats `runnerd`'s empty `required`. Works even when
  no generic runner exists — the bootstrap situation.
- **Dependency-ordered with a live answerer.** Because a core item's expr names
  its *real* builder (e.g. cargo's is `run /std/flake-builder -- --in:@=.`),
  *forming* cargo's key dispatches `flake-builder`'s build — which must already
  be answered. So the seeder cannot compute all keys independently up front: one
  thread polls-and-answers from the current seed map while the main thread
  extends the map in order (`flake-builder → runner → cargo → rustc →
  deep-deps`). Forming key *N* only dispatches items `< N`, already registered.

### Breaking the cycles

1. **`flake-builder`'s self-reference** (resolution-time, in-process, before any
   dispatch the seeder could answer). Fix: `flake-builder`'s `.caos-expr` names
   its image as the sentinel `docker://seeded` — `run docker://seeded -- --in:@=.`.
   A `docker://` ref passes straight through `resolve_expr_image` (needs a small
   addition — that resolver doesn't yet handle `docker://`; `resolve_run_image`
   already does), so evaluation never re-enters `/std/flake-builder`. The formed
   key is `{ image: docker://seeded, in: <flake-builder tree>, std, salt }`,
   which is exactly what the seeder registers and answers. Nothing pulls
   `docker://seeded`: the seeder answers first; if it is absent, the job hits
   `runnerd` and fails loudly on a bogus image (an honest "bootstrap broken"
   signal, never a silent wrong answer). Only `flake-builder` uses the sentinel;
   every other core item names its real builder.
2. **The top-level `deep-deps` `.caos-expr`** (if the tree root deepens the whole
   tree, forming any std key needs `deep-deps`, which needs `rustc`/`cargo` — a
   cycle). Fix: bootstrap works against a **de-deep-deps'd tree** (the top-level
   `deep-deps` call stripped), hand-fills the deepened deps itself, and seeds.
   Once `deep-deps` is seeded, callers using the *real* top-level expr form the
   same keys.

### Build order (first cut)

1. `docker://` passthrough in `resolve_expr_image`; `flake-builder`'s `.caos-expr`.
2. Seed-record format + `bootstrap.sh` emitting records (key from caos-expr eval
   → hand-built artifact), still hand-building the core as today.
3. `core-seeder-runner`; start it in `stack/serve` alongside the server.
4. Give the remaining core entries (`runner`, `cargo`, `rustc`, `deep-deps`)
   their `.caos-expr`; wire the dependency-ordered live answerer.
5. Hand-deepening + its byte-identity guardrail test.

### Landed (steps 1–3 + flake-builder converted)

- **`docker://` passthrough** in `resolve_expr_image` (`crates/caos/src/eval.rs`):
  a `docker://<ref>` image token is returned verbatim, no dispatch — so
  evaluating `flake-builder`'s expr never re-enters `/std/flake-builder`.
- **Seed-record format + emission** (`build-builtins.sh`): after publishing
  `refs/caos/std`, bootstrap publishes `refs/caos/seed` — a tree
  `{ <name> -> { required: <blob JSON name→oid>, result: <object> } }`. Today's
  sole record is flake-builder's:
  `required = { image: <blob "docker://seeded">, in: <flake-builder source> }`,
  `result = <hand-built flake-builder image delta>`. `required` deliberately
  **omits `std`/`salt`** (flake-builder's output depends only on `in`), so the
  seeder answers under any std — which is what lets the test harness hand each
  job a std *subset*. The server matches `required` as a **subset** of a job's
  arg entries, so this still beats runnerd's empty required.
- **`core-seeder-runner`** (`crates/server/src/bin/core-seeder-runner.rs`, a bin
  in the `server` crate — reuses its `gix`/`minreq`/`serde_json`/`caos-world`
  deps, no new workspace member): reads `refs/caos/seed` from the server's git
  repo, long-polls `/runner/poll` (one poll per record, `required` = the
  record's entries), and posts the pre-built result on a match — no container,
  no server change. Wired into `stack/serve` behind `CAOS_STACK_SEEDER`, on for
  the host and the test stacks, off for the seed (publish-only) placement.
- **flake-builder converted**: its std entry is now a **source tree** (with the
  `.caos-expr`); its hand-built delta is the seed **result**. Two resolution
  paths reach it and both go through the seeder:
  - *client eval* (`resolve_std_image` → eval `run docker://seeded` → dispatch →
    seeder), for `/std/bash`, `/std/merge`, `eval-path`, etc.;
  - *server `resolve_flake_image`* (`crates/server/src/compute.rs`), which
    `worker-common`'s `std_image("bash")` reaches by handing the server a raw
    bash flake tree. `resolve_flake_image` now resolves the *builder* image via
    `flake_builder_image`: a delta std entry is used directly (old form), a
    source entry is resolved by forming `run docker://seeded -- --in:@=<source>`
    and dispatching it (the seeder answers). This is the one place the server
    had to change — it can't eval a `.caos-expr`, so it forms the single
    flake-builder key it knows.
  - The flake-builder **worker** (`std/flake-builder/worker`) now self-references
    via `/cas/args/image` (its own resolved image), not `/cas/std/flake-builder`
    (which, as a source entry, would hand the server a raw flake to rebuild and
    cycle).
- **Harness seed plumbing**: `build.sh`'s `make` stage pushes `refs/caos/seed`
  to the outer server and emits a `seed` build output; `test.sh` stage3 hands
  each test wrapper that seed; `test-stack/worker` fetches it into the inner
  stack (a git fetch, like std) so the inner `core-seeder-runner` has the
  record. Seeded into **every** wrapper (unlike the per-test std subset), since
  any test that builds a flake reaches flake-builder transitively.

Whole suite green (28/28) with flake-builder resolved through the seeder.

### Landed (step 4, in progress): deep-deps via local mounts, no ambient std

- **`resolve_expr_image` evaluates a path-referenced subtree** (`crates/caos/src/eval.rs`):
  an image token that is a path now resolves through that subtree's own
  `.caos-expr` (a subtree with none evaluates to itself). This is what lets a
  core item name a dependency by a **local** path — a deep-deps mount at
  `DEEP-DEPS/<name>` — instead of `/std/<name>`, resolving it the same way
  `/std/<name>` would. Tested in `tests/eval-path` (`run tool` over a subtree
  that carries its own curry `.caos-expr`, converging on the byte-identical
  arg-tree as the direct form).
- **`std/deep-deps` converted to a hand-deepened source entry.** Its checked-in
  form is `{.caos-expr = "curry DEEP-DEPS/runner -- --worker1:@=worker", DEPS =
  "../runner runner"}`; `build-builtins.sh` publishes it **hand-deepened** —
  `{.caos-expr, worker, DEEP-DEPS/runner}` with the compiled worker staged and
  the `../runner` dep mounted as the runner delta — so it names its runner base
  by a local mount, not ambient `/std/runner`. A pure `curry` (assembly only),
  so its key needs nothing seeded. Resolving `/cas/std/deep-deps` evaluates the
  expression to the **byte-identical** curry node it used to publish directly
  (hash-preserving refactor; suite green).

**The key realization (yours):** the hard part isn't runtime or currying — it's
**computing the seed key**. Bootstrap forms each item's key by running the real
evaluator (so the key matches the caller's), and a `run DEEP-DEPS/<worker>` in an
expr forces resolving that worker *to an image*, which for a seeded worker means
it must already be seeded. So key computation is necessarily **interleaved with
seeding, in dependency order** — the live answerer — for `run` items. `curry`
items (deep-deps, rustc) are pure assembly and need no answerer.

**Refinement that removed the live answerer entirely:** bootstrap doesn't have
to *run* the evaluator — it **hand-assembles** each key from the deltas it
already built (as the flake-builder seed does). cargo's key is
`{ image: <flake-builder delta>, in: <deepened cargo entry> }`; both are known
at bootstrap, so `build-builtins.sh` constructs `required` directly, seeds it,
and needs **no seeder running during publish** and **no dependency-ordered
interleave**. The seeder just reads one complete `refs/caos/seed` at stack
bring-up. (The risk hand-assembly trades for is reproducing the caller's key
exactly; the suite is the check.) This also means the seeder-refresh /
drop-the-poll change is a nice-to-have, not required for correctness — the
rescan already converges on a static, complete record set.

### Landed: cargo (a seeded `run` item)

- `std/cargo` is now a hand-deepened source entry
  (`.caos-expr = "run DEEP-DEPS/flake-builder -- --in:@=."`,
  `DEPS = "../flake-builder flake-builder"`). `build-builtins.sh` still
  hand-builds the cargo image delta; that delta is the seed **result**, keyed on
  `{ image: <flake-builder delta>, in: <deepened cargo entry> }`. Resolving
  `/cas/std/cargo` evaluates the expr → resolves `DEEP-DEPS/flake-builder`
  (seeder → flake-builder delta) → forms cargo's key → seeder → the cargo delta,
  with the flake-builder worker never running. cargo resolves to the same delta
  as before (backward-compatible); the ~10 cargo-using tests stay green.

### Landed: all runner-pool workers converted; the curry loop deleted

Every compiled worker that rode `curry(runner, worker1=<binary>)` is now a
**hand-deepened source entry** `{.caos-expr, worker, DEEP-DEPS/…}`, assembled
uniformly (`assemble_pool_worker` in `build-builtins.sh`):

- `bash-tool`, `llm-call`, `llm-step`, `deep-deps`: `curry DEEP-DEPS/runner --
  --worker1:@=worker` (DEPS `../runner runner`). Pure curry, hash-preserving —
  resolving each yields the byte-identical node it used to publish directly.
- `rustc`: `CARGO=curry DEEP-DEPS/cargo --` then `curry DEEP-DEPS/runner --
  --worker1:@=worker --cargo=$CARGO --worker-common:@=worker-common` (DEPS
  `../runner runner`, `../cargo cargo`). The `CARGO` variable resolves the
  `DEEP-DEPS/cargo` mount to the cargo image (a mount is an image only in image
  position), and `worker-rustc` now reads `--cargo` as that tree-ref's hash
  (`cas_hash`) instead of blob content — the one code change rustc needed.

The `build-builtins.sh` **curry loop is gone**; there is no per-worker
special-casing left, and none of these entries names a dependency by ambient
`/std` any more.

### North star (agreed): kill `/std` entirely

No ambient `/std/...` anywhere. A consumer project (future) fetches std via a
worker that expands a GitHub URL into a tree, pulls caos through a **root-level
`.caos-expr`**, and refers to std items through **deep-deps** mounts. In the
meantime, *this* project reaches std through deep-deps (`DEEP-DEPS/x`).

### Landed: one deepen pass; ambient `/std` gone from all std exprs

- **`build-builtins.sh` runs a single `deepen_entry` pass.** Each std entry is
  assembled UN-deepened (checked-in source; compiled workers stage their binary
  as `worker`), then one recursive, memoized pass replaces every entry's `DEPS`
  with a `DEEP-DEPS/` subtree of its deepened dependencies (siblings named
  `../<name>`), sharing identical subgraphs by hash. This **replaced all the
  per-entry hand-deepen splices** (`assemble_pool_worker`, the bespoke
  cargo/rustc/flake-builder blocks) — landed byte-identical (every test hash
  unchanged), then extended below.
- **Every std expr names its deps via `DEEP-DEPS/x`** (or the `docker://seeded`
  sentinel), never ambient `/std`. `bash`/`merge` build via
  `DEEP-DEPS/flake-builder`; `rgrep` via `DEEP-DEPS/rustc` + `DEEP-DEPS/runner`;
  the rest already did. (The only `/std/` left in std trees is in explanatory
  comments.)
- **`worker-rustc` skips caos metadata** (`DEEP-DEPS/`, `.caos-expr`, `DEPS`)
  when laying out a `--src` project — the wrinkle the deepen-into-the-entry model
  creates: nix ignores extra dirs, but cargo would choke on the nested
  `Cargo.toml`s under `DEEP-DEPS/`. A rgrep-shaped tool's cargo project is
  therefore identical before/after the flip (compile is a cache hit); only
  bash/merge re-key their flake build once.

### Remaining (this project)

- **`bash-tool` source-migrated** ✓: `crates/worker-bash-tool` deleted; `std/bash-tool`
  is now `{Cargo.toml, src/, .caos-expr, DEPS}` built via `DEEP-DEPS/rustc` (like
  `rgrep`, worker-common-only so no bake impact); the ~5 `CAOS_BIN_DIR` tests now
  resolve `/cas/std/bash-tool` instead. (One gotcha for future migrations: a std
  entry passed to a *worker* as an image ref — e.g. llm-step's `--bash-image` —
  must be **resolved to a built hash first** (`curry /cas/std/X --` in the caller,
  or `resolve_cli_image` in `chat.rs`); a worker handed the raw `/cas/std/X` path
  resolves it to the source entry, not the built image.)
- **Still binary-staged** (`stage_worker`): `llm-call`, `llm-step`, `deep-deps`.
  `llm-call`/`llm-step` await step 2 (they carry crates.io deps + `llm-client`);
  `deep-deps` stays seeded/hand-staged per cycle-2.
- **Cargo bake union via an anchor crate** (step 2) ✓: `crates/bake-anchor` is a
  dependency-only workspace crate holding the std tools' crates.io deps (`regex`,
  `serde_json`, `minreq{https-rustls}`), so the `--workspace` `/std/cargo` bake
  keeps vendoring+precompiling them once the tool crates leave the workspace.
  `lint-bake-anchor.sh` (run by `tests/std-lint`) enforces `anchor ⊇ every std
  tool's crates.io deps` by name; version/feature parity stays on the build-time
  bake-reuse guard. No `bake.nix` change (it's already `--workspace`).
- **crates.io + `llm-client` leaves** (step 3) ✓: `llm-call`/`llm-step` are now
  source std entries built via `DEEP-DEPS/rustc`; `llm-client` is its own std
  entry (`std/llm-client`, a lib, no `.caos-expr`) mounted into both via `DEPS`
  and linked as a code dep. `worker-rustc` gained **numbered `--dep0/--dep1/…`**
  args (repeated `--dep` would collide at `/cas/args/dep`), each a crate tree it
  expands one level, reads the `[package] name` from, and splices at that name —
  so a tool's manifest stays natural (`llm-client = { path = "llm-client" }`).
  `crates/{worker-llm-call,worker-llm-step,llm-client}` deleted; the ~5
  `CAOS_BIN_DIR` tests now resolve `/cas/std/llm-{call,step}` (or, for chat, the
  default `LLM_STEP_IMAGE`). Workspace now holds only the seeded core's crates +
  `worker-common` + `bake-anchor` + `llm-stub`.
- **`stage_worker` is down to 2 users**: `deep-deps` and `rustc`. Both are core
  (seeded in the finale); everything else is source-built or a flake now.

### Finale (remaining)

- **`rustc` + `deep-deps` seeded** ✓; **`stage_worker` deleted.** Each checked-in
  entry is now a `{.caos-expr}` sentinel (`run docker://seeded-rustc` /
  `run docker://seeded-deep-deps` — distinct sentinels so their keys don't
  collide); bootstrap hand-builds the curry (`curry(runner, worker1=<binary>[,
  cargo, worker-common]`) as the SEED RESULT and seeds it under the sentinel's
  key. No binary is injected into the checked-in tree, so a consumer fetching it
  resolves them via the seeder. (`worker-rustc`'s `--cargo` reverted to a hash
  blob — the seed result binds it directly, so the `$CARGO`-variable/`cas_hash`
  machinery is gone.) Seeded core: **flake-builder, cargo, rustc, deep-deps**.
- **`runner` is NOT seeded and is not a source entry** (decided): it is brought
  up by caosd, and nothing else needs it. `std/runner` stays the raw host-built
  delta that `DEEP-DEPS/runner` mounts. This drops the whole
  `--runner`/`own_image()` cascade the earlier sketch needed.

### Landed: the flip — std is deepened at RESOLVE TIME by the real worker

This is the step the whole phase was for: **the checked-in tree is now
self-resolving, and std resolves by descent.**

- **`std/.caos-expr`** (new): `run deep-deps -- --in:@=.`. Every
  `/cas/std/<name>` resolution (`resolve_std_image` → `eval_std_entry` →
  `eval_path`) applies it before descending to the entry, so an entry's
  `DEEP-DEPS/<dep>` mounts are computed by the real `deep-deps` worker instead of
  being baked into the ref.
- **Cycle-2 is broken by `resolve_expr_image`'s subtree evaluation**, with no new
  machinery: the image token `deep-deps` is a *path*, and a path image is
  evaluated **against its own subtree**, so it sees `std/deep-deps`'s
  `docker://seeded-deep-deps` sentinel (answered by the seeder) and never
  re-enters the root expression. Nor does the transform's *result* re-enter it:
  `eval_path` evaluates a node's `.caos-expr` and then descends into the result
  without re-checking the result's own root, so the deepened tree's surviving
  root `.caos-expr` is inert.
- **`build-builtins.sh` publishes the un-deepened source** — every entry exactly
  as checked in, `DEPS` and all — under a std root carrying that `.caos-expr`.
  `deepen_entry` survives for **one** purpose: forming **seed keys**, since a
  seeded item's arg-tree `in` is its *deepened* entry and bootstrap must know
  that hash before any stack exists to compute it.
- **Byte-identity, and how much of it is actually load-bearing.** The hand-deepen
  must match the worker or the seeder registers a key no caller forms. The
  exposure is narrower than it looks: of the seeded core, only **`cargo`** has a
  `DEPS` file — `flake-builder`, `rustc` and `deep-deps` are DEPS-free, so their
  deepened form is identity. So the one tree that must agree is cargo's
  `{bake.nix, flake.lock, flake.nix, .caos-expr, DEEP-DEPS/flake-builder}`.
- **Guardrail: the suite, deliberately** (over a standalone worker-context diff
  test, which is awkward to plumb — `cli.sh` sees std only as a resolver and the
  inner stack holds a subset). A divergence is not silent: the formed key stops
  matching the seeded one, the job falls through to the generic runner, dies on a
  sentinel image, and every test that resolves the item goes red — which
  localizes to the item, since the items are independent.
- **The cost, accepted: every test's `uses-std` widened.** Resolving *anything*
  now runs the transform over the test's std **subset**, so each subset must be
  closed under `DEPS` and must additionally name `deep-deps` (the transform) and
  `runner` (the seeded deep-deps result is a curry whose `base` is a hash *blob*,
  so the runner tree does not ride along with the seed). `caos-tools/test.sh`
  symlinks `std/.caos-expr` into every subset. Per-test incrementality drops a
  little — a `runner` or `deep-deps` edit now re-keys every test — but the lists
  are still honest: those are real dependencies now, not ambient ones.

### Remaining

- **Retire the `/cas/std/*` scaffolding.** `refs/caos/std` + `resolve_std_image`
  + the `/cas/std/<name>` vocabulary are now a *naming* convention over a tree
  that resolves perfectly well by plain descent. Replacing them with tree-path
  descent (`./std/foo`, or `./inputs/caos/std/foo` in a consumer) is the end
  state — and the last thing standing between this project and the "kill `/std`
  entirely" north star above.
