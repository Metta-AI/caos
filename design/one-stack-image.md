# caosd is the stack — design note

**Status:** PROPOSED. Supersedes the compose half of the host deploy and the
hand-written bring-up in `test-stack/worker`. Depends on the image/worker
contract in `flake-images.md`; `test-stack-image.md` is the direct ancestor —
this note generalises its "the stack is an image" claim from the test path to
*every* path.

## Problem

**There are two hand-written bring-ups.** The host's is a compose file plus
~180 lines of `caosd` (`load_once`, `check_current`, `compose_up`,
`compose_up_diagnose`) standing up four containers. The test stack's is
`test-stack/worker`, which starts redis, the server and runnerd as three
processes in one container. They must agree on every daemon's configuration —
redis addressing, git dir, runner slots, registry naming, the socket grant —
and nothing makes them agree. They can drift silently, and when they drift the
suite is testing a stack shape the host never runs.

**`build-builtins.sh` has three callers and runs ~20 times per suite.** Once on
the host (`caosd up`), once as `tests/lib/warm-std.sh`, and once inside every
per-test stack (`tests/lib/run-test.sh`). The warm-up exists only to stop a
cold registry from producing a thundering herd (`test-stack-image.md`, "Std is
published per test"). Three call sites, two configurations, one of them a
scheduling workaround.

**A wiped registry fails confusingly** — as nineteen simultaneous per-test
failures deep inside the fan-out, not as one clear statement that the registry
no longer has what std references.

**macOS forces containers anyway.** Every workspace crate builds for musl, so
`server` and `runnerd` are Linux ELF; `BUILDING_ON_MACOS.md` is explicit that
"the server, runner, and workers run in Linux containers". Any design where the
host runs daemons as bare processes needs a second design for Darwin.

## The model

**`caosd` *is* the stack.** Not a tool that manages one — the process that
supervises the daemon group: start redis, the server, runnerd and the registry,
wait for each, and die as a unit when any of them dies. Configured from the
environment and nothing else, so it is correct wherever it is put.

**The container is the caller's decision, not caosd's.** Sometimes something
starts a container to run caosd in; sometimes caosd is already in one and just
runs. Either way it is the same caosd, with the same configuration surface, and
the daemon group has exactly one implementation in the tree.

That splits the verbs cleanly:

```
caosd serve   BE the stack, here, in this container/process
caosd up      host convenience: start the stack image as a container
              whose command is `caosd serve`, then wait for it
```

`up` is what a human types. `serve` is what the stack is. Tests and the seed
call `serve` directly, because they are already inside a container and starting
another one would be the only thing they gained.

**The image is the same one either way.** The root flake's
`packages.<system>.caosImage` already contains a complete caos stack built from
the tree — the binaries, the userland the daemons shell out to (git, redis,
skopeo, the docker client, diffutils, gawk), and the tree's clean core image
tarballs. `up` runs that image. The suite runs that image. The host stack
becomes a test stack that persists, which is the whole point: the suite stops
approximating the host and starts running it.

### macOS is not a special case

The daemon group runs inside a container on **both** Linux and macOS — on Linux
because `up` puts it there, on macOS for the same reason. There is no
host-process placement, so there is no second design for Darwin and no
divergence in what CI, a Linux dev box and a Mac exercise. The cost is one
container hop on Linux; it buys a single code path.

## Placements

`serve` is the constant; who calls it, and which members the group has, is
configuration.

| | host (`up` → `serve`) | test (image `/worker` → `serve`) | seed (flake derivation → `serve`) |
|---|---|---|---|
| **redis** | persistent under `$CAOS_DATA` | private, empty, dies with the job | throwaway |
| **server** | ✓ | ✓ | ✓ |
| **runnerd** | ✓ | ✓ (delegates to the outer engine) | **none** — publishing runs no workers |
| **registry** | ✓, port published | none — uses the host's, on purpose | none — uses whichever is reachable |
| **afterwards** | stays up | runs `worker1`, publishes `/tmp/out` | `build-builtins.sh`, then exit |

Two of those deserve their reasons stated, because they look like omissions:

**The test placement runs no registry** deliberately. A content-addressed
registry shared across tests "cannot leak behavior, only work"
(`test-stack-image.md`) — sharing it is what stops every test rebaking the
toolchain. Redis is the opposite: private and empty, because the incrementality
tests assert real memoization and a shared result cache poisons them.

**The seed placement runs no runnerd** because `build-builtins.sh` runs no
workers — see "The seed" below. That is what makes the seed legal inside the
flake-builder container, which holds no socket grant.

### The netns knob

Workers must share a netns with the server that dispatched them. `runnerd`
already takes `CAOS_DOCKER_NETWORK` (`crates/runnerd/src/main.rs:115`) and the
test stack already sets it to `container:<self>`. With the daemons always in a
container, **every placement sets it the same way**, and `caos-net` disappears
along with the container-name addressing (`http://caos-server`) that only ever
existed to serve compose.

That also collapses a naming split for the host: with the registry inside the
group and its port published, `localhost:5000` is correct both from inside and
from the engine pulling host-side. The split survives only for the test
placement, which reaches the host registry across a netns boundary — so
`CAOS_REGISTRY_HTTP` stays, with exactly one consumer instead of being a
general parameter.

## caosd's surface

```
caosd serve       be the stack here (blocks; dies as a group)
caosd up          start the stack image as a container running `serve`
caosd down        stop it, keep $CAOS_DATA
caosd logs        tail the group's logs
caosd reset       stop and wipe $CAOS_DATA
caosd std-build   publish std to this stack's registry and git, then exit
caosd std-check   verify what std references still exists; non-zero if not
```

**Die as a group.** Any member's death takes the group down, so a half-dead
stack is never something anyone has to diagnose. Compose restarted services
individually; this does not. That is the "die when there's an unexpected error"
stance applied to a daemon group, and it is deliberate.

**`up` pushes; `std-check` is the strict gate.** A human running `up` on a
machine whose registry has never seen the streamed images should get a working
stack, not homework — and the stack image already carries the clean tarballs,
so `up` can push what is missing without recomputing anything. The test runner
instead calls `std-check` once, before the fan-out, and a wipe is one clear
error up front rather than nineteen confusing ones inside. Convenience for
people, strictness for the suite.

`std-check` resolves `refs/caos/std` and confirms every digest it names is
still in the registry — **through the same name the engine will pull with**,
and it reports which name it checked. A check that passes over one name while
`docker pull` fails over another is precisely the confusing failure this
replaces.

**Logging.** `docker logs` on one container interleaves four daemons.
`test-stack/worker` already writes `/tmp/{redis,server,runnerd}.log`; the host
placement writes `$CAOS_DATA/logs/` and `caosd logs` tails them.

## The seed

`std-build` is callable from the flake's seed derivation, so std can be
published *at image build time* and baked into the image as a git dir:

```nix
seededGit = runCommand "caos-seeded-git" { } ''
  CAOS_GIT_DIR=$out caosd serve --seed &
  bash ${./build-builtins.sh}
'';
```

`#caosImage` carries `$out`; a placement points `CAOS_GIT_DIR` at it instead of
creating an empty one. The binaries doing the publishing are the ones the same
flake just built, so "the inner std is published by the tree's own
`build-builtins.sh`" holds more strictly than today — provably once per tree
rather than once per test.

This works because **`build-builtins.sh` runs no workers.** `cli_curry`
(`crates/caos/src/lib.rs:2411`) resolves the image, constructs tree entries and
`ensure_pushed`s them with `git push <hash>:refs/caos/req/<hash>`
(`lib.rs:435-443`). Flake entries are `cp -RL` + `git write-tree`. Streamed
entries are an image push plus a curry over the digest. The whole script is
bookkeeping and image pushes — so the seed needs a server and registry write,
but **no runnerd, no engine socket, and no grant widening**. The flake-builder
container holds no socket grant (`std/flake-builder/flake.nix` sets only
`CAOS_WORKER_UID=0`), and it does not need one.

It can host the seed at all because its nix runs unsandboxed — "single-user,
unsandboxed nix: root in a container with no nixbld group and no privilege to
build a sandbox", `--option sandbox false` (`std/flake-builder/worker:59-63`) —
so a builder has the container's network and can reach the registry.

### What the seed does not cover

**Redis stays empty in the test placement**, and it should. Publishing writes
git objects, refs and registry blobs; redis holds *job results*, and no job
runs. There is nothing for the seed to put there.

So the per-test cost the seed does **not** remove is the first *use* of each std
entry inside a fresh stack — the flake-builder job that images `std/bash`, and
the equivalent for every other entry. Those are use-time costs the publish
never populated, so removing the per-test publish neither helps nor hurts them.

Carrying them would mean *running* the std workers at seed time, which needs
runnerd and therefore the engine socket — a grant the flake-builder container
does not hold and should not gain. That is a separate design with a real price,
and it is out of scope here.

### The purity trade, stated

Today the image is a pure function of the tree: the memo tag is the tree hash
(`test-stack-image.md`, "Who builds what"). A seeded image is a function of
(tree, registry contents) while the tag still captures only the tree. The
failure that buys:

> wipe the registry → the image memo still hits → you get an image whose baked
> std names blobs that are gone → and re-running never fixes it, because the
> memo is still valid.

`std-check` at bring-up is the answer, and it is a verified precondition rather
than a speculative fallback: assert the invariant, die with a specific message,
and let `std-build` (or, for a human, `up`) be the explicit repair.

## What this deletes

- the compose file (~110 lines of `flake.nix`)
- `serverImage`, `runnerdImage`, `serverContents`, `runnerdContents`,
  `serverConfig`, `runnerdConfig`, `loadServer`, `loadRunnerd` — the daemons
  stop being two separately-built images
- `load_once`, `check_current`/`stale`, `compose_up`, `compose_up_diagnose` and
  the `docker load` content-tag memo in `caosd`
- `caos-net` and container-name addressing (`http://caos-server`)
- `tests/lib/warm-std.sh` entirely, and `run-test.sh`'s publish block
- the second bring-up: `test-stack/worker`'s daemon section becomes
  `caosd serve`

## What this does not change

- **docker is still the worker engine.** `runnerd` launches every worker with
  `Command::new(&config.docker_bin)` — both of its two launch paths
  (`crates/runnerd/src/main.rs:158,279`). There is no process backend in the
  code; `cargo-workers.md:275` calls one "the no-socket fallback", but that is
  design intent, not something runnerd implements. This note removes
  docker-as-daemon-host, not docker.
- **The socket grant is unchanged in scope.** The stack container needs the
  engine socket because runnerd is inside it — the same grant runnerd holds
  today, and the same per-image grant the test stack already declares.
- **`caos does not build caos`** (`flake-images.md`). The core still builds from
  the host with `nix build`.

## Decisions taken

- **Die as a group**, above.
- **`up` pushes**, above — humans get convenience, the suite gets `std-check`.
- **No state migration.** The new `$CAOS_DATA` layout is not compatible with
  compose's bind mounts, and that is fine: this lands with a one-time `caosd
  reset`. (The redis dir is owned by a host subuid, so that reset still needs
  `sudo rm -rf` or `podman unshare` — as `caosd reset` already documents.)

## Open question

**`docker build` must go first.** The flake-builder container has no daemon, so
`build-builtins.sh`'s streamed path has to move onto skopeo plus the
server-side git-docker delta the flake-builder already emits (`{base:
docker://<clean>, config.json, layer00}`, which "the server converts into a
digest"). This is a prerequisite for the seed placement, not an optional
speedup — and it independently removes measured waste on the host path: a miss
extracts a 99 MB tarball (**3.8s**) into a **286 MB** build context, while only
**3 of 100 layers — 585 KB of 111 MB — actually differ** between consecutive
tags. What is unresolved is whether the delta emit belongs in
`build-builtins.sh` or is borrowed wholesale from the flake-builder.

## Sequence

1. **`caosd serve`** — extract `test-stack/worker`'s daemon section into it;
   `test-stack/worker` calls it. No behaviour change, and the suite proves it.
2. **skopeo conversion** — `build-builtins.sh`'s streamed path off `docker
   build`. Verifiable through the existing `caosd up` path alone.
3. **`caosd up` runs the image** — compose, both daemon images and `caos-net`
   delete. The biggest single step, and the one that pays off even if nothing
   after it lands.
4. **`std-build` / `std-check`** — split the publish out of `serve`; `up`
   pushes, the test runner calls `std-check` before the fan-out.
5. **The seed** — `std-build` in the flake's seed derivation; `warm-std.sh` and
   `run-test.sh`'s publish block delete.

Steps 1–3 stand alone: they buy the single bring-up and the deletions without
touching how std is published. 4–5 are what make `build-builtins.sh` a
build-time step with one caller.
