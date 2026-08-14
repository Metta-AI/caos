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

**Every runnable image carries the *caos additions***: the setuid
`/bin/caos` client, the `worker` user (uid 1000), a writable `/tmp`, and
`/usr/bin/env` — the things that change with every caos build (the client)
or that an image build can't produce (setuid). For a flake, caos itself
adds them (the stack stage below); a git-docker tree carries them authored
in by its producer (the suite's image jobs do), with `<name>.caosmeta`
sidecars encoding what git modes can't — the convert applies the sidecars
either way.

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

> **Superseded by `caos-expr.md` (2026-08).** The server no longer detects
> flakes: `resolve_flake_image` and its by-name builder lookup are DELETED.
> A flake directory carries a `.caos-expr` (`run --base:@=DEEP-DEPS/flake-builder
> --in:@=.`) and the CLIENT evaluates it, so what reaches the server is
> already an image. The stage machinery below is still what
> `std/flake-builder/worker` does; only the entry point changed.

`resolve_image` (server `compute.rs`): `docker://` passes through; a flake
tree goes to `resolve_flake_image`; anything else hex is a git-docker tree.

`resolve_flake_image` runs **`std/flake-builder`** over the flake tree (via
the server's own `run_image` sub-run). One script, three curried stages
(`std/flake-builder/worker`):

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
| literal flake tree | `bash`, `testenv` | the checked-in `std/<name>` directory, copied whole — `{flake.nix, flake.lock, worker}`, nothing generated |
| staged flake tree | `runner`, `cargo` | the checked-in files + the nix-built `/worker` binary staged on top (`std/<name>/stage-tree.sh`) |
| curry | `bash-tool`, `llm-call`, `llm-step`, `rgrep`, `rustc` | `curry(runner, worker1=<binary>)` |

A flake can only read its own tree, and resolution cannot strip a tree
(only the flake knows which files its build reads) — so each std tree
carries exactly the build's inputs and nothing else, and its hash (= the
image's cache key) never moves for irrelevant edits. The literal trees ARE
their `std/<name>` directories; the staged trees add only the one thing
that cannot be checked in, a nix-built binary.

- **`runner`**: flake + lock + the `worker-runner` interpreter binary
  (staged).
- **`bash` / `testenv`**: flake + lock + the script runner as `./worker` —
  `std/bash/worker` is the source of truth, `std/testenv/worker` a
  byte-identical checked-in copy, and the test suite's image jobs read the
  `std/bash` copy. testenv adds git/redis/the docker client and the
  `CAOS_WORKER_UID=0` grant — per-image containment policy for
  nested-stack jobs.
- **`cargo`**: flake + lock + checked-in copies of the workspace's
  manifests, `Cargo.lock`, `rust-toolchain.toml`, empty stubs at each
  crate's real target paths (cargo and crane's `mkDummySrc` detect
  autodiscovered targets by file presence), and the staged `worker-cargo`
  binary. No source — a source edit never re-keys the toolchain bake; a
  worker-cargo/worker_common edit does, and pays one cold rebake.

Every `flake.lock` is **derived from the main flake.lock** — checked in,
not generated: `std/refresh.sh` (re)writes each std lock from the root
lock's nodes, and `tests/std-lint` runs the same script in `--check` mode
(re-derive, byte-compare), so a std flake's pins cannot drift from the
root's unnoticed — which keeps `std/cargo`'s rustc the exact compiler that
builds caos (the caos-in-caos suite depends on that). The same
refresh/check pair covers every checked-in redundancy: the testenv worker
copy and `std/cargo`'s vendored workspace inputs. `std/cargo`'s bake
machinery lives in `std/cargo/bake.nix`, imported by BOTH that flake and the
root flake (for `cargoDepsImage`, the suite's deps-only base) — one
definition, no drift.

### rustc: the worker factory

`std/rustc` compiles ONE worker's source and wraps it as a worker. It is
pure orchestration — no toolchain in its image — published as
`curry(runner, worker1=worker-rustc, cargo=<std/cargo's tree>,
worker-common=<crates/worker-common>)`. The bound args are its linker
inputs: it lays the user source out as a cargo project linking
`worker-common`, `run-then`s the compile into the `cargo` ref, and its
output is `curry(<caller-supplied runner ref>, worker1=<built binary>)` — a
worker returning a worker. The wiring is invisible at the call site by
design (callers pass only `--src` and `--runner`); this section is where
it's written down.

## Bootstrap

The flake-builder is the one image that can't be *built* by the flake path
— resolution would recurse into itself. (Nothing stops its *definition*
from being a flake tree; the grounding is about who runs the build, not
the notation — and its definition IS a flake:
`std/flake-builder/{flake.nix, flake.lock, worker}`, whose clean
`#caosImage` carries everything but the caos additions.) It's host-nix-built
SELF-CONTAINED — nix, CA certs, and tools from its own nixpkgs, the closure
registered in the store db (`includeNixDB`) so in-image builds work — and
**streamed with the additions COMPOSED ON**: the root flake takes the
clean `#caosImage` as-is (calling the subflake's outputs function), and
build-builtins stacks the caos additions over it as a CONTENT-KEYED tar
layer (`FROM scratch`, ADD per layer — extraction as root preserves the
setuid caos), pushes, and memoizes on (clean tarball store path, composed
binaries' content). That is the same clean-image + additions-delta shape
the stack stage gives every flake image — one additions model, two
encodings (a git tree with `.caosmeta` sidecars in the stack stage; a
mode-carrying tar here). The runner and cargo stream the same way, their
`/worker` (worker-runner, worker-cargo) riding as a second content-keyed
layer — the host is those images' author. Because the clean images depend
on nothing from the workspace and the deltas key on the binaries' BYTES, a
Rust edit that leaves them bit-identical re-streams nothing and re-keys
nothing downstream (verified: a server-only edit is a registry hit for
both). Cargo is where that is worth the most: its clean image is 3.4 GB of
toolchain and baked deps, and while its 5 MB `/worker` was baked in,
*every* Rust edit re-tarred and re-gzipped all of it under `nix build`
(~25s) and then gunzipped and re-pushed it under `caosd up` (~16s). So
resolution never re-enters the flake branch for the builder, and the
recursion is grounded. (The stock-base pins live with their only
consumer: `caos-tools/{nix,debian}-base.ref`, the test suite's own image
pipeline, until it unifies onto the flake path.)

## Part 2: literal trees, worker-built binaries (phase A shipped)

The agreed next pass. The principle: **a simpler build and a stronger
check** — the build generates nothing, and `run-tool test` verifies every
redundancy the simplification introduces.

**Phase A (SHIPPED, 2026-07): literal checked-in std trees.** One script,
`std/refresh.sh`, regenerates every checked-in redundancy from its source
of truth; `tests/std-lint` runs the SAME script in `--check` mode
(regenerate, byte-compare) inside the suite, so the check cannot drift
from the generator. Drift becomes impossible to miss instead of impossible
to create. Per tree:

- Every `flake.lock` is **checked in**, derived from the root lock by the
  refresh script (which replaced `std/lib/derive-lock.sh`); rerun it on
  pin bumps.
- **`bash` / `testenv`** carry the worker script in the flake directory —
  `std/bash/worker` the source of truth, `std/testenv/worker` a verified
  byte-identical copy (a flake reads only its own tree), the suite's image
  jobs (`caos-tools/build.sh`) the third consumer, reading the `std/bash`
  copy. Their `stage-tree.sh` generators are GONE: the two directories ARE
  their published trees, copied whole by `build-builtins.sh`. `jq` joined
  their userlands (the lock check needs it; generally useful).
- **`cargo`** checks in copies of the workspace manifests, `Cargo.lock`,
  `rust-toolchain.toml`, and the target stubs, refresh-generated and
  lint-verified; its `stage-tree.sh` shrank to staging only the
  `worker-cargo` binary onto the checked-in files.

**Still open in this pass:**

- **`runner`** builds its `/worker` from source in-tree: worker-runner +
  worker-common have zero external deps, so the flake vendors the two
  crates and builds them purely, no network. The tree re-keys on their
  source — rare and correct. (Vendor-vs-move is undecided; until then its
  `stage-tree.sh` stages the nix-built binary onto the checked-in
  flake + lock.)
- **`worker-cargo`'s binary origin** — same question, deferred with it.

With those, the remaining `stage-tree.sh` generators go too; publish
collapses to "hash the checked-in trees, build the curries."

**`images/` dissolves into `std/*`** — every std entry gets its
directory, the flake-builder included:

- `std/flake-builder/` is a literal flake tree (DONE): `flake.nix` + lock
  + `image.nix` + `worker`, self-contained — nix + certs from its own
  nixpkgs, `includeNixDB` for in-image builds, no nix.conf needed (the
  worker passes every option explicitly). The feared closure push
  measured ~100MB, streamed in seconds; `base.ref` is gone (the stock
  pins moved to their only consumer, `caos-tools/`). `images/` no longer
  exists.
- `images/bash-worker.sh` → the checked-in `std/bash` / `std/testenv`
  worker copies above (DONE, phase A); the suite's image jobs reference
  the `std/bash` copy until they unify.
- `images/debian-base.ref` dies with the suite's second pipeline (Open
  items below).

**Workers build the curry binaries.** The host builds only the core — the
`caos` client, `caosd`, the cli, and the flake-builder image (`nix build`
from a blank slate is canonical; `nix develop` + stateful cargo is the dev
loop). Everything else std needs is built by workers, each defined as a
flake, a curry, or the result of calling another worker:

- worker-common-only binaries are exactly `std/rustc`'s input shape (one
  `.rs` + worker-common): compiled in-caos, curried onto runner, memoized
  per source edit.
- `llm-call`, `llm-step`, and `rgrep` carry crates.io deps (`serde_json`/`minreq`/
  `regex`), which rustc refuses — they need the cargo-backed path.
  Collapsing the cargo/rustc pair into one "build worker" is the remaining
  open design here.

**Test fixtures leave std** (DONE, phase B). `file-count`, `dirs-only`,
`deep-deps` had no consumers outside their own tests, and `hello` none
outside `examples/consumer` — tests in disguise. Each test now carries its
fixture's `.rs` (`tests/<name>/worker.rs`) and builds it with `std/rustc`
at test start (memoized: one compile per source edit); `hello.rs` rides in
`examples/consumer/` the same way. A test needing a fixture *image* can
carry a flake and invoke the flake-builder. std holds only entries with
real consumers: `flake-builder, runner, cargo, bash, testenv, rustc,
bash-tool, llm-call, llm-step, rgrep`.

**caos does not build caos.** The core builds from the host, from a blank
nix slate — for an immature project, reasoning about which features the
*builder* stack has versus the tree under test is a tax on every change.
caos-in-caos stays what it is today: a test workload
(design/cargo-workers.md) proving caos builds real projects, never the dev
build path. (An earlier direction — std generated by tree-transformation
workers over a seeded stack — is superseded: literal trees need no
transformation at all.)

## Open items

- Registry GC of built images.
- The nested test stack builds its own runner/bash/cargo images from the
  tree under test (delta-over-pinned-debian, `caos-tools/lib/`) — a second
  image pipeline, and a userland (debian) that production std no longer
  uses. Unify onto the flake path once std trees are literal (Part 2).
- ~~Boundary caching for two-level images (a cheap worker layer over an
  expensive base, both flake-defined) — today `std/cargo` accepts a full
  rebake when its `/worker` changes.~~ **Closed (2026-07-29)**: not by
  caching the boundary but by removing it — `std/cargo`'s image is CLEAN
  like the runner's, and `/worker` is composed on at publish as a
  content-keyed layer (Bootstrap, above). The bake is keyed on (toolchain,
  manifests, lockfile) and nothing else, so no Rust edit rebakes or
  re-pushes it.
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
