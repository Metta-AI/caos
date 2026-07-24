# Flakes as first-class images — design note

**Status:** proposed. Decided in a design discussion (2026-07). Generalizes the
phase-2 **flake-worker** from `runner-pool-and-cloud-builds.md` (written for the
now-dead fly backend — read "in a worker" wherever it says "on fly") from a
worker you invoke by hand into an **image form** that image resolution builds on
demand. Pairs with `durable-resolution.md` for the build memo.

## Goal

Make any directory that contains **both `flake.nix` and `flake.lock`** runnable
exactly like a git-docker image directory is today. You reference the flake tree
where you'd reference an image; resolution converts it to a real image and runs
it. No hand-invoked build step, no pre-published image.

This is the same currency split as `runner-pool-and-cloud-builds.md`: git/CAS
carries the *source* (the flake tree, tiny); the registry carries the *built OCI
blob*; the two meet at a `docker://…@sha256` digest that caosd already runs and
already stacks as `base =`. The flake form adds **no new concept to caosd's run
path** — only a new branch in resolution that produces a digest.

## Resolution: a third image form

`resolve_image` (server `compute.rs`) today:

- `docker://ref` → used as-is;
- hex hash (git tree) → `convert_git_image`.

Add, before `convert_git_image`, a test on the tree root:

```
tree has flake.nix AND flake.lock at root
    → resolve_flake_image(tree)                     # builds it, returns a registry digest
else
    → convert_git_image(tree)                       # unchanged git-docker form
```

Both files are required. A flake dir without a lock is **rejected**, not built —
determinism (a stable digest) is the whole premise, and an unlocked flake has none.

`resolve_flake_image` runs `std/flake-builder` over the flake tree (via the
server's own `run_image` sub-run primitive), which returns a **git-docker delta
tree** `{base: docker://<clean>, +caos}`; it then `convert_git_image`s that delta
into a registry digest. Because `resolve_image` needs `std`/`salt`/`stack`/`trace`
to launch the sub-run, those thread down from its one caller (`run_dispatch`).

**Implemented:** `crates/server/src/compute.rs` — `resolve_image` gains the flake
branch, `is_flake_tree` (both-files probe), `resolve_flake_image` (run + convert),
`std_image` (look up a name in the std tree).

## Bootstrap (catch 1): the seed builder is host-built

The **flake-builder** is itself an image, so "resolve a flake by running the
flake-builder" looks circular. It isn't, because the builder is referenced by a
**docker digest**, never by a flake:

- The **seed** flake-builder is host-nix-built by `build-builtins.sh` and
  **streamed straight to the registry** — its layer tarball is composed onto
  the stock nixos/nix base with `docker build` (ADD extracts each layer as
  root, preserving the setuid caos) and pushed; `std/flake-builder` is then a
  tiny **curry node over the digest ref**, so none of its ~75 layers ever
  enter git. The registry tag, keyed on the tarball's store path, memoizes
  the compose.
- Building a *nested* stack (caos-in-caos, the test stack) builds **that stack's**
  flake-builder using the **host's** flake-builder. One level of self-hosting per
  stack; the recursion terminates at the host.

Invariant that keeps it bounded: **exactly one image — the flake-builder — is
referenced by digest; every other image may be referenced by a flake dir.**
Resolving a flake dispatches a run whose *image* is the flake-builder, which
resolves to a digest (not a flake), so resolution never re-enters the flake
branch for the builder.

(Renamed from "nix-builder": it builds *images from flakes*, and nix is an
implementation detail of how.)

## Clean key (catch 2): two workers, inner clean, outer stacks caos

The expensive nix build must be keyed **without `/caos`** so a caos edit never
re-triggers it. This is the strip-caos trick in `caos-tools/lib/build-stage2.sh`
(`rm usr/bin/caos; cp /bin/caos` — done there to keep the nixbuilder key stable),
generalized into two workers:

- **`std/flake-builder-inner`** — produces the **clean image** (no caos in the
  *output*), though the worker image itself **does carry a setuid `/bin/caos`**
  (it must, to run `caos hash`/`get`/`put`). A worker image on stock
  `docker://nixos/nix` (via `import-image --base`, so nix + its store stay stock
  registry layers). `nix build`s the flake's `#caosImage`, pushes it to the
  registry, and returns the clean image as **`{ref, config}`** — the digest plus
  the flake image's OCI config.

  **The clean image is not rebuilt when the caos binary changes** — but not
  because the inner is caos-free. A caos change re-keys the inner worker's
  request, so it re-runs; the re-run is cheap because two **caos-independent**
  caches short-circuit the expensive work: the **registry tag `flake-<H>`**
  (`H = caos hash <flaketree>` — a git tree hash, identical no matter which caos
  computes it) and the **nix store**. On a hit the inner skips the `nix build`
  and the push and just returns the existing digest. So a caos edit costs one
  inspect-and-return here; only the cheap stack-stage caos delta re-stacks.

**Implemented as ONE self-contained builtin** (`std/flake-builder`,
`images/flake-builder.sh`, image `workerFlakeBuilderImage` in `flake.nix`): a
single script branching on a curried-in `--stage`:

- **orchestrate** (default): `run-then` the **build** stage over the flake
  tree, with the **stack** stage as the `then`.
- **build**: the "inner" role above — `nix build` the flake's `#caosImage`,
  stream it to the registry, return the clean image as `{ref, config}`.
- **stack**: emit a git-docker delta `{base: docker://<clean>, config.json,
  layer00: /bin/caos setuid 4755 (via a `.caosmeta` sidecar) + /bin/caos symlink
  + **/worker runner trampoline** + /tmp + userdb}`. The server converts that
  (cheap, one caos layer on a docker base) → **`digestWorker`**.

The trampoline in the delta is what lets a flake `#caosImage` be a **pure
base** — no `/worker`, no caos, stable hash — and a worker be
`curry(<flake image>, bin=<binary>)`, the runner-pool model.

Why the inner returns `config` too: a real flake worker (e.g. rustc) keeps its
toolchain at nix-store paths only its own OCI config names. Stage 2 must carry
that config into the stacked image's `config.json` (convert uses **our**
config.json, not the base's), forcing the runner entrypoint and appending `/bin`
to PATH. The inner does that rewrite (it has the config + `jq`); the massaging is
a pure function of the flake config and never perturbs the clean image or its tag.

Consequence: a caos edit re-runs **only** the cheap outer stage 2; the inner's
expensive build is a registry hit. The clean image is what you'd `base =` from
other flakes; `digestWorker` is what you *run*.

## Credentials

The inner worker pushes to the local caos registry over HTTP (`skopeo
--insecure-policy --tls-verify=false`), so no creds today. When the registry
needs auth, curry a token in as a bound arg (it rides the args tree = CAS);
long-lived is fine for now (single-tenant), narrow to a short-lived
registry-scoped token later, per `runner-pool-and-cloud-builds.md`.

## Build memo

No separate flake memo is needed: the inner's `nix build` is skipped by the
**registry tag** `flake-<treehash>` (durable, self-healing across restarts), the
outer run is single-flighted + Redis-cached by the compute machinery (keyed on
the request, which includes the builder image), and the `convert_git_image` of
the delta is cached on the delta-tree hash. A repeated resolve is a lookup; a caos
edit re-keys only the cheap outer run (inner still hits the registry tag). Under
`durable-resolution.md` the outer run is just a pending node, single-flighted by a
Redis lease.

## std entries as flakes: cargo-base (finding B)

The end-state is **most std entries are git trees containing flakes**, built by
the flake-builder and streamed to the registry — ideally one built-in (the
flake-builder) and everything else defined as a flake. `std/cargo-base` is the
pilot, because it was the heaviest git-imported image (toolchain + baked deps):

- **`std/cargo-base/flake.nix`** (checked in) replicates the main flake's
  cargo machinery — toolchain from the workspace's `rust-toolchain.toml`
  (`minimal` + musl target), crane vendor + `buildDepsOnly` (musl, dev),
  musl cross cc — as a `#caosImage` that carries `/ws-root`, `/target-dir`
  and the inflated writable `target/`, the same worker Env as before, and
  **no `/worker`, no caos** (the builder's delta supplies those).
- **The published tree is GENERATED at publish** (`std/cargo-base/stage-tree.sh`,
  run by `build-builtins.sh`): the flake + the workspace's
  `rust-toolchain.toml`, `Cargo.toml`/`Cargo.lock` and member manifests, plus
  **empty stubs at each crate's real target paths** (cargo only sees
  autodiscovered targets whose files exist; crane's `mkDummySrc` detects them
  the same way and overwrites the contents). No source — so a source edit
  never re-keys the tree and the registry memo `flake-<H>` holds.
- **The flake.lock is DERIVED from the main flake.lock** in `stage-tree.sh`
  (jq: same nixpkgs/rust-overlay/crane nodes, a root naming just those).
  This is the toolchain-pin coupling: the cargo worker must compile with the
  exact rustc that builds caos (the caos-in-caos suite), and deriving the lock
  makes drift structurally impossible.
- `std/cargo` stays `curry(std/cargo-base, bin=worker-cargo)` — curries peel
  before image resolution on both the client and `run_image` paths, so the
  flake branch sees the bare tree with no new server code.
- The old `cargoBaseImage` git-import is gone (`flake.nix`,
  `builtinWorkerImages`, `build-builtins.sh`). The deps-bake machinery stays
  in the main flake for `cargoDepsImage`, the test suite's D2 deps-only base.

## The rest of std follows

With cargo-base proven, the remaining std entries converted to the same
shapes (`build-builtins.sh` header lists the entry forms):

- **`std/bash-base`, `std/testenv-base`** — flake trees (checked-in
  `flake.nix` + a lock derived from the main lock by `std/lib/derive-lock.sh`;
  fully static, so their stage-tree.sh is two lines). `std/bash` and
  `std/testenv` are `curry(<name>-base, bin=images/bash-worker.sh)` — the
  script runner is just a bin like any other. The caos delta gained a
  `/usr/bin/env` symlink so env-shebang script bins run on bare nix bases.
- **`hello`, `file-count`, `dirs-only`, `deep-deps`** — no images at all
  anymore: `curry(runner, bin=worker-<name>)`, the same runner-pool move the
  agent-harness bins always used. Self-recursion through map-then works
  under a curry (the bound `bin` rides the args tree into every child).
- **`std/runner`** — a flake tree too: a pinned-nixpkgs userland (bash +
  coreutils + grep/sed/find/tar/gzip — the agent-visible environment for
  bash-tool), replacing the old thin delta on stock debian. The old `base`
  entry (debian + caos, pre-runner-pool) had no consumers and is deleted.
- So the **streamed `flake-builder` is the ONLY nix-built worker image**;
  its stock base (nixos/nix) rides as registry layers via the docker-build
  compose. Everything a client reaches through std is a flake tree or a
  curry over one.

## Open items

- Registry GC of built images — same concern as today's worker images
  (`runner-pool-and-cloud-builds.md`).
- Where `dispatch_build` waits: inline blocking today (fine for the prototype),
  the durable pending-node model later.
- Flake detection is a tree-root probe; confirm it composes with `--base` /
  currying inputs that also carry a tree (the flake test is on the *image* input's
  root only).
- Substituter reachability from the inner worker (cache.nixos.org or a private
  substituter); consistent with `worker-network-stance` (pin via lock, network
  allowed for pinned deps).
