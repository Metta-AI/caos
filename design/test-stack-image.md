# The test stack as an image — design note

**Status:** agreed, not yet built. Closes the second-pipeline open item in
`flake-images.md` and supersedes the nested-stack half of
`cargo-workers.md` (phases 3–4 stay as history). Depends on
`flake-deps-image.md`. Read the image/worker/flake contract in
`flake-images.md` first.

## Problem

There are two image pipelines in this tree. The host's is the flake path:
`nix build`, `std/flake-builder`, clean image + caos additions. The test
suite's is a second one — `caos-tools/lib/{image-build,bake,build-stage2*}.sh`
stacking deltas onto pinned `debian`/`nixos-nix` bases, with the workspace
binaries handed in from the host as `refs/caos/bins`. So the suite exercises
an image-building path that production no longer uses, over a userland
production no longer ships, from binaries the tree under test did not compile
inside caos.

`tests/lib/run-nested.sh` then stands up an inner stack per test from those
pieces, publishing an inner std by hand (`git mktree` + curries) — a third
expression of what `build-builtins.sh` already does on the host.

## The model

**One image is the test stack.** The root flake's `packages.<system>.caosImage`
is an image that contains a complete caos stack built from the tree under
test: the server, runnerd, the client, every worker binary, redis, git, the
docker client, skopeo — and the tested `std/flake-builder` image, baked in as
a tarball payload. Its `/worker` is an interpreter in the sense
`flake-images.md` already defines: it reads the next executable from `worker1`
and runs it — after bringing the inner stack up.

Everything else follows from that:

- The **host** builds the core with `nix build`, as today, and nothing about
  the dev loop changes. `caos does not build caos` (`flake-images.md`) still
  holds for the dev build path; this is the test workload.
- **`caos-tools/build.sh` collapses to one job**: hand the workspace tree to
  `/cas/std/flake-builder` and get a digest back. No stages, no bases, no
  bins tree.
- **`caos-tools/test.sh` maps over the tests**, running that image once per
  test with the test's tree as `in` and a test runner as `worker1`.
- The runner **prepares, then runs the test's own `cli.sh`** against the inner
  stack.

## Two caoses, one image

The image carries two caos clients, and they never collide because the
distinction is exactly the distinction between the two stacks:

- `/bin/caos` — the setuid gateway the **additions** layer stacks in, composed
  by the host from the host's binary. This is what makes the image a worker on
  the *outer* stack: the interpreter uses it to fetch args and `caos put` the
  result, and it is what the ambient environment resolves to throughout.
- the **tested** `caos` / `caos-cli` at `/caos/bin`. The interpreter never
  puts these on its own `PATH`. It sets them at the call site and nowhere
  else, with `CAOS_STD` and `CAOS_SALT` scrubbed (the outer run's values must
  not reach the inner client — `cargo-workers.md`, phase 3).

**Keeping them straight is two independent choices, and both matter.** *Which
binary* decides whose semantics and protocol you speak: the outer server and
runnerd are the host's, while the tested client is the thing under test and
may differ from the host's in any way — that is the entire point of building
it, so the two are interchangeable only by coincidence, never by assumption.
*Which URL* decides which server that binary then talks to.

The failure modes are asymmetric. Right binary, wrong URL fails loudly:
`/bin/caos` under the flipped environment asks the inner server for an outer
object and 404s (measured — this is how the first cut broke). Right URL,
wrong binary fails **silently**: a host client driving the test stack passes
every test until the tree under test changes the client, and then it is
quietly testing the wrong thing.

So neither is left to discipline. `/caos/bin` is never on the ambient `PATH`
(the image sets `PATH=/bin`), so nothing outside the call site can reach the
tested client; and `worker1` is given no outer work to do. Concretely the
interpreter:

1. materializes every arg (`caos get -r /cas/args`) while the environment is
   still the outer one,
2. runs `worker1` with `PATH` / `CAOS_SERVER_URL` / `DOCKER_HOST` set at the
   call site,
3. publishes `worker1`'s `/tmp/out` as the job's result afterwards.

`worker1` therefore never needs an outer client, which is why it cannot hold
the wrong one. That is one documented divergence from `std/bash`, whose
`worker1` does its own `caos put /cas/out`.

The same split propagates for free. The baked `std/flake-builder` image
carries the **tested** caos as its gateway, because the root flake composed it
from the binaries it just built. Every image that flake-builder then stacks
inherits the tested caos by the existing default, so no stage needs a `--caos`
argument and no image ends up with a client from the wrong stack.

## Who builds what

**The host builds:** the core binaries, and the one bootstrap image the flake
path cannot build because it *is* the flake path — `std/flake-builder`, from
the tree under test. It rides inside the test-stack image as a tarball.

**The test stack builds everything else.** At bring-up the interpreter seeds
the baked flake-builder into the registry under a content-keyed tag, and from
there the inner stack builds each remaining std entry (`bash`, `testenv`,
`cargo`, the `rustc` and bin-worker curries) with its own flake-builder,
against its own server. Nothing about the outer stack's version can influence
what those images contain: the memo tag is the std flake tree's hash, and each
std flake carries its own lock, so the artifact is a pure function of the tree
under test.

That purity is what makes the next point safe.

## The registry is a shared cache, redis is not

The inner stack uses the **host's registry** (`caos-registry:5000`) rather than
a private one. It is a content-addressed cache keyed on tree hashes, so
sharing it across the outer stack, across tests, and across runs cannot leak
behavior — only work. Without it, the first flake-builder job in every test
would rebake the toolchain.

The **result cache stays private**: a per-job redis that starts empty and dies
with the job, exactly as `run-nested.sh` does today. The incrementality tests
assert real memoization and a shared redis poisons them.

**The naming rule bites in both directions here** (the gotcha recorded in
`flake-images.md`'s appendix). The stack container sits on `caos-net`, so its
own pushes and the inner server's pulls use `caos-registry:5000`; but any image
it asks the **outer** engine to run must be named `localhost:5000`, because
that daemon pulls host-side. One process now touches both halves.

## Std is published per test — measured, not assumed

This note originally specified a `build-std.sh` job: build std once per suite,
hand each test the digests. That was written to avoid nineteen tests each
firing a flake-builder job per std entry just to probe registry memos.

**Built instead:** each per-test stack publishes its own std by running the
tree's `build-builtins.sh`. Two reasons, in order of weight:

1. Handing a test a *digest* requires extracting a resolved image digest, and
   resolution is server-side. Reproducing it client-side would duplicate
   server logic on a guess — a mechanism invented to serve an unmeasured
   optimization.
2. The overhead it was meant to avoid does not show up. A full 19-test suite
   runs in **~7 minutes wall clock** with 16 stacks concurrent, each doing a
   complete std publish. Per-test bring-up is not the bottleneck.

If it ever becomes one, the fix is the original plan, and it needs a real
digest-extraction seam first.

## The socket grant becomes per-image

The inner runnerd delegates to the outer engine through a bind-mounted socket,
as `cargo-workers.md` phase 4 established — nested podman remains the wrong
trade. What changes is who gets the socket. Today `CAOS_RUNNER_SOCKET` grants
it to **every** worker in the pool, which the compose file flags as coarse.
Now there is exactly one image that hosts a stack, so the grant becomes a
property that image declares in its own env, the same shape as the existing
`CAOS_WORKER_UID=0` containment grant, and runnerd honors it per-image. That
closes phase 4's "refine the socket grant from pool-wide to per-image".

## Cache keys

A per-test job keys on `(test-stack image digest, std digests tree, test tree,
runner script)`. A source edit moves the image digest and re-keys every test —
which is what a binaries change already does today, so nothing regresses. The
`std-manifest` closure rules in `suite-stage3.sh` exist to keep an unrelated
worker edit from re-keying every test; with one image carrying the whole
stack that distinction is gone, and the manifests go with it.

## Why depsImage comes first

Without `flake-deps-image.md` implemented, the test-stack image is one
`flake-<H>` memo over the whole workspace tree: every source edit is a total
miss that rebuilds the dep closure and the toolchain inside the container
before a single test runs. With it, an edit rebuilds only the workspace
crates. The same applies to `std/cargo`, whose tree carries real
`worker-cargo`/`worker-common` source. It is a prerequisite, not an
optimization.

## What this deletes

- `caos-tools/lib/{image-build,bake,build-stage2b,build-stage2c,build-final}.sh`
- `caos-tools/{debian-base,nix-base}.ref` — and with them the last debian
  userland in the tree
- `tests/lib/run-nested.sh`, `tests/lib/suite-stage3.sh`'s closure rules, and
  every `tests/*/std-manifest`
- the `--bins` interface: `refs/caos/bins` stops being an input to the build
  and test tools

## Costs and edges

- **Anything published into git must be dereferenced.** `buildLayeredImage`
  links `contents` into the image root, so the tree at `/caos/tree` is
  symlinks into `/nix/store/…-caos-test-stack-root/…`. Those resolve fine for
  reading here, but `build-builtins.sh` copies std trees into a client repo
  and `git add`s them — copied verbatim, the publish produces a tree of links
  to store paths no other container has, and the flake-builder then reports
  "no flake.nix in the flake tree". Hence `cp -RL` at both staging sites. The
  tell was there earlier and worth remembering: the inner `cargo` tree hash
  never matched the host's for the same files. Tarballs and binaries are
  unaffected — they are read and executed in place, never copied into a tree.
- **The image's userland is the publisher's userland.** `build-builtins.sh`
  runs *inside* the test stack now, so every command it shells out to has to
  be in the image — `gawk` was missing (coreutils has no `awk`) — and every
  tree it stages has to be there too, which is why `crates/worker-common`
  rides alongside `std/` (`std/rustc` curries that source in).
- **Setuid in a baked tarball.** The baked flake-builder needs a setuid
  `/bin/caos`. `build-builtins.sh` composes additions with docker partly
  because nix strips setuid when it seals a store path;
  `fakeRootCommands` writes the mode into the layer tar before sealing, so it
  likely survives, but it is unverified. Fallback if not: the interpreter
  composes the additions with skopeo at seed time — the host already does
  exactly this.
- **The first run is cold and honest.** The first suite after a registry wipe
  bakes the toolchain inside a test stack. Every later run is a tag hit.
- **Bring-up cost per test** is today's ~20s cold / ~70ms hit, unchanged in
  shape: an unchanged test never starts a stack at all.
- **The image is large** — it carries a stack plus a bootstrap image — but it
  is layered, and the expensive layers are keyed on the lockfile, not on
  source, so a source edit re-pulls only the thin layers.

## Sequence

1. **DONE** — `depsImage`: the second memo in the flake-builder's `build`
   stage, with `std/cargo` as its first consumer (`flake-deps-image.md`).
2. **DONE** — root flake `#caosImage` = the test stack (binaries, userland,
   the baked flake-builder, the interpreter `/worker`) and `#depsImage` over
   the source-independent half.
3. **DONE** — the rewritten `build.sh` / `test.sh` / `suite*.sh` and
   `tests/lib/run-test.sh`.
4. **DONE** — deleted the second pipeline, the `.ref` pins, `run-nested.sh`,
   and the manifests.
5. **Open** — per-image socket grant in runnerd, declared via image env.
   Today's pool-wide `CAOS_RUNNER_SOCKET` still hands the socket to every
   worker; now that exactly one image hosts a stack, the grant can move into
   that image's env like `CAOS_WORKER_UID=0` does.

Measured on the way: the test stack builds in caos in **4m32s** cold from a
bare container (dep bake, workspace compile, ~400MB image assembly and push);
a full 19-test suite runs in **~7 minutes**.
