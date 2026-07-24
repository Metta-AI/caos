# Images, workers, and flakes — the contract

**Status:** SHIPPED (2026-07). This note is the authority on what an image is,
what a worker is, and how flakes become runnable — the vocabulary is
deliberately docker's, with no new concepts beyond it.

## The model

**caos runs docker images.** An image can be written down three ways:

1. a **`docker://` ref** — used as-is;
2. a **git-docker tree** — `{config.json, base?, layerNN/…}`, converted by the
   server into a registry digest (`convert_git_image`);
3. a **flake** — a git tree whose root holds `flake.nix` **and** `flake.lock`
   (both required: an unlocked flake has no stable identity). Nix builds the
   image; caos memoizes the build (below).

**caos adds its layer to every image it builds from a flake**: the setuid
`/bin/caos` client, the `worker` user (uid 1000), a writable `/tmp`, and
`/usr/bin/env`. These are the *caos additions* — the things that either change
with every caos build (the client) or can't ride a git tree (setuid).

**A flake defines everything about the image except the caos additions.**
`/worker` — the executable `caos runner` execs — included. caos NEVER
installs a `/worker`: an image whose definition has one is a **worker
image**; an image without one is just an image (a base, an environment), and
running it fails plainly.

**An interpreter image** is a worker image whose `/worker` runs its argument
— exactly `python:3` or `bash` on Docker Hub. The exec-chain argument names
are the **workerN convention**: anything fetched and executed is `workerN`,
where N is its depth from the image's `/worker`; data args keep domain names
(`cmd`, `tree`, …). `std/runner`'s `/worker` fetches `worker1` and execs it
(compiled workers); `std/bash`'s `/worker` runs `worker1` with bash
(scripts). A curried interpreter at `worker1` would read `worker2`.

**A curry is an image ref plus saved args** — partial application, a tiny
CAS tree `{base, args, .caos-curry}`, never an image build. Currying is
STRICT: rebinding an already-bound name is refused (run-time args may still
override — that remains the call-args-win rule). Most std workers are
curries: `std/hello = curry(std/runner, worker1=worker-hello)` — one shared
runner image, one warm pool, per-worker cost of one small blob.

## Resolution: how a flake becomes a digest

`resolve_image` (server `compute.rs`): `docker://` passes through; a flake
tree goes to `resolve_flake_image`; anything else hex is a git-docker tree.

`resolve_flake_image` runs **`std/flake-builder`** over the flake tree (via
the server's own `run_image` sub-run). One script, three curried stages
(`images/flake-builder.sh`):

- **orchestrate** → `run-then` build, then stack.
- **build**: check the registry for tag **`flake-<H>`** (`H` = the tree's
  git hash — the durable, caos-independent memo); on a miss, `nix build
  <tree>#caosImage` in-worker, stream the result to the registry, return
  `{ref, config}` (config massaged: runner entrypoint forced, `:/bin`
  appended to PATH).
- **stack**: emit a git-docker delta `{base: docker://<ref>, config.json,
  layer00: the caos additions}` — which the ordinary convert turns into the
  final digest.

Every step is memoized (registry tag, Redis request cache, convert cache):
a repeated resolve is a lookup; a caos edit re-runs only the cheap stack.

The build stage runs nix unsandboxed (a worker container can't nest the
build sandbox — the flake's pinned lock carries reproducibility instead).
Two consequences it handles: single-user mode (`--option build-users-group
"" --option sandbox false`, `HOME=/tmp`), and builders that write `$HOME`
(go builds) littering `/homeless-shelter`, which nix never cleans — the
build retries, removing the litter, ONLY on that exact error (completed
drvs stay in the store, so retries make monotonic progress).

## std

Publishing (`build-builtins.sh`, run by `caosd up`) has three entry forms:

| form | entries | what ships |
|---|---|---|
| streamed | `flake-builder` | nix-built, composed onto stock `nixos/nix` with `docker build`, pushed; the entry is a curry over the digest ref — no layer bytes in git |
| flake tree | `runner`, `cargo`, `bash`, `testenv` | a generated tree: the checked-in flake + a derived lock + whatever the build reads |
| curry | `bash-tool`, `llm-step`, `rgrep`, `rustc`, `hello`, `file-count`, `dirs-only`, `deep-deps` | `curry(runner, worker1=<binary>)` |

The flake trees are **generated at publish** (`std/<name>/stage-tree.sh`)
because a flake can only read its own tree, and resolution cannot strip a
tree (only the flake knows which files its build reads). Each generator
assembles exactly the build's inputs — and nothing else, so the tree's hash
(= the image's cache key) never moves for irrelevant edits:

- **`runner`**: flake + lock + the `worker-runner` interpreter binary.
- **`bash` / `testenv`**: flake + lock + `images/bash-worker.sh` as
  `./worker` (one source of truth with the test suite's image jobs).
  testenv adds git/redis/the docker client and the `CAOS_WORKER_UID=0`
  grant — per-image containment policy for nested-stack jobs.
- **`cargo`**: flake + lock + the workspace's manifests, `Cargo.lock`,
  `rust-toolchain.toml`, empty stubs at each crate's real target paths
  (cargo and crane's `mkDummySrc` detect autodiscovered targets by file
  presence), and the `worker-cargo` binary. No source — a source edit never
  re-keys the toolchain bake; a worker-cargo/worker_common edit does, and
  pays one cold rebake.

Every `flake.lock` is **derived from the main flake.lock** at publish
(`std/lib/derive-lock.sh`), so a std flake's pins structurally cannot drift
from the root's — which keeps `std/cargo`'s rustc the exact compiler that
builds caos (the caos-in-caos suite depends on that). `std/cargo`'s bake
machinery lives in `std/cargo/bake.nix`, imported by BOTH that flake and the
root flake (for `cargoDepsImage`, the suite's deps-only base) — one
definition, no drift.

### rustc: the worker factory

`std/rustc` compiles ONE worker's source and wraps it as a worker. It is
pure orchestration — no toolchain in its image — published as
`curry(runner, worker1=worker-rustc, cargo=<std/cargo's tree>,
worker_common=<crates/worker-common>)`. The bound args are its linker
inputs: it lays the user source out as a cargo project linking
`worker_common`, `run-then`s the compile into the `cargo` ref, and its
output is `curry(<caller-supplied runner ref>, worker1=<built binary>)` — a
worker returning a worker. The wiring is invisible at the call site by
design (callers pass only `--src` and `--runner`); this section is where
it's written down.

## Bootstrap

The flake-builder is the one image that can't be a flake (it's what builds
flakes). It's host-nix-built as a thin layer set over stock `nixos/nix`
(pinned by digest in `images/nix-base.ref`) and **streamed**: composed with
`docker build` (ADD extracts each layer as root, preserving the setuid
caos), pushed, memoized on the tarball's store path. Exactly one image is
referenced by digest; everything else may be a flake — so resolution never
re-enters the flake branch for the builder, and the recursion is grounded.

## Open items

- Registry GC of built images.
- The nested test stack builds its own runner/bash/cargo images from the
  tree under test (delta-over-pinned-debian, `caos-tools/lib/`) — a second
  image pipeline, and a userland (debian) that production std no longer
  uses. Unify onto the flake path when std-as-transformation lands.
- Boundary caching for two-level images (a cheap worker layer over an
  expensive base, both flake-defined) — today `std/cargo` accepts a full
  rebake when its `/worker` changes.
- Registry credentials: local HTTP today; curry a token as a bound arg when
  auth arrives.

## Appendix: how it got here (history, kept for archaeology)

- Mechanism + `resolve_flake_image` seam, the cargo-base pilot ("finding
  B"), the one-bake refactor, streaming the flake-builder, and the std
  conversion landed as separate commits on `flake-images` — see `git log`.
- The delta originally installed a "runner trampoline" at `/worker` into
  every flake image, and flakes were "pure bases" that couldn't define
  their worker. That inverted the docker-native contract and scattered the
  runner's identity across three homes; it was replaced by
  always-self-defining worker images (this note's contract). The `bin` and
  `script` args became `worker1` at the same time, after a real collision
  bug (a caller's `--bin` silently replacing the script runner) motivated
  both the workerN convention and strict currying.
- Operational gotchas that cost real debugging, preserved: value args are
  lazy placeholders (`caos get` before reading); the clean-image ref the
  build stage returns must be the ON-NET registry name
  (`caos-registry:5000`), while curries over streamed digests use the
  host-facing `localhost:5000` (runnerd pulls via the host daemon);
  `fetch_base` needs `--src-tls-verify=false` for the HTTP registry; new
  std files must be git-added before `caosd up` sees them (`${self}` is the
  tracked tree only).
