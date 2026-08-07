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
