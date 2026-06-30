# Worker runner pool + cloud image builds — design note

**Status:** design, agreed shape, not yet built. Committing to **debian-slim /
gnu** for phase 1; musl + a flake-built toolchain are the phase-2 follow-on.

**Branch:** `runner-pool`, off `main` (which has the docker-base-layers feature:
git-docker images that stack on a stock `docker://` base). NOTE: the fly backend
itself — dispatch, the OCI-layout convert + `fetch_base_oci`, per-worker machine
RAM (`caos.fly.memory-mb`), and the `run_std`/`std_tree` threading fix — currently
lives on `fly-serve-backend`, not on `main`. So phase 1 is developed and measured
on the **dev/Docker backend**; integrating with fly (and reconciling those
commits) is an explicit later step.

---

## Problem this solves

Measured on fly: the worker *machine* path (provision + 6PN dispatch + pull +
run) is ~6s and healthy. What's slow is that **every produced worker is its own
docker image** — so each novel worker is: convert (git-docker → OCI) → push to
registry.fly.io → provision a new per-version fly app → pull. The push alone is
~16s, dominated by registry **per-operation latency** (a ~1 KB config blob takes
~4s; it's round-trips, not bytes), and the base layers ride along since the
glibc shift made produced workers debian-based.

The fix is not to optimize that push — it's to **stop minting an image per
worker**.

---

## Principle: `docker://` is the single currency for built images and bases

- **Git/CAS** carries *source and thin deltas* — small, content-addressed,
  dedupable (worker code, the curry args, the caos delta layers). This is what it
  is good at.
- **The registry** carries *big built OCI blobs* — toolchains, bases. This is
  what it is good at.
- A big image is **never** forced through git. It is built (by nix, on fly),
  streamed straight to the registry, and referenced by digest like any
  `docker://` ref. caosd already runs `docker://` refs directly and already
  stacks them as `base = docker://…`, so this needs **no new concept** in caosd.

---

## The three pieces

### 1. Runner pool — the cold-start fix (phase 1)

A single, stable image with a **warm pool** of machines:

- Base: `docker://debian:stable-slim` + the setuid `caos` (a thin delta, via the
  docker-base-layers mechanism already on `main`).
- A generic `/worker` **trampoline**: read the `bin` arg (a CAS blob = a compiled
  worker binary), write it out, `chmod +x`, and `exec` it. The exec'd binary *is*
  the real worker — it uses `worker-common` exactly as today (reads `/cas/args`,
  writes `/cas/out`).
- "Run worker X" becomes: dispatch to a warm runner machine + `curry bin=X`.
  **No per-worker image, no convert, no push, no provision.** Cold-start collapses
  to warm-dispatch + a small blob fetch over 6PN + exec.
- gnu-dynamic worker binaries run on the debian-slim runner (libc matches). musl
  (phase 2) would let the runner be scratch/minimal and the binary run anywhere.
- Bonus: caosd's per-run work drops to *resolve + dispatch + serve the blob* —
  the per-worker convert+push disappears, which also answers "caosd is doing a
  lot."

### 2. rustc as a *producer of binaries*, not images (phase 1)

- The rustc worker compiles user source to a single binary (gnu-dynamic for now),
  `caos put`s it, and returns the blob — instead of assembling a worker *image*.
- The runnable worker is `curry(/cas/std/runner, bin=<blob>)`.
- rustc itself stays a stock-`rust:1-bookworm`-based image (docker-base-layers),
  warm-pooled — its size/first-run is a one-time, amortized cost, not per worker.

### 3. flake-worker — cloud image builds (phase 2, the musl enabler)

A general "build me an image from a flake, in the cloud, addressed by hash":

- Base: `docker://nixos/nix` (stock) + a thin caos delta — host-built but tiny,
  the only heavy-ish thing that ever crosses the home link, and it rides
  docker-base-layers.
- Input: a flake ref (in git, tiny) + **curried-in registry push credentials**
  (decided: pass them as a bound arg). It `nix build`s the image *on fly* (pulling
  nix deps from cache.nixos.org — fly-fast), and **streams it straight to the
  registry** (`streamLayeredImage | skopeo`, so the big tarball needn't even land
  on disk). `/out` is just the resulting **docker digest**.
- The digest is used like any `docker://…@sha256:…` ref — run it, or `base =` it.
- Determinism: nix builds are reproducible, so the digest is stable and caos
  memoizes the *run* → build-once. (Requires a pinned `flake.lock`.)
- This is what lets a **full musl toolchain** rustc image be built on fly without
  a home transit and without a hand-published custom image — but it is worth
  having regardless of musl (custom/pinned bases, the server image itself, …).

---

## Trust / security

The runner execs an arbitrary binary as the unprivileged worker user; `caos`
stays the setuid-root gateway to `/cas`. This is the **same** boundary as today —
the binary already *is* the worker — only now it arrives as a CAS blob rather than
baked into an image.

Curried push creds (piece 3) are stored in the args tree, i.e. in CAS. For a
single-tenant stack that is acceptable; harden by currying a **narrow,
short-lived, registry-scoped** token rather than the org deploy token.

---

## Phasing

- **Phase 1 (this branch, debian-slim/gnu):** the runner pool + rustc-as-producer.
  Delivers the cold-start win, is fully testable on the dev/Docker backend, and
  needs neither musl nor the flake-worker.
- **Phase 2 (follow-on):** the flake-worker, a flake-built musl-toolchain rustc,
  and a scratch/minimal runner. Decouples us from "what a stock base happens to
  ship."

## Open items

- Integrate with the fly backend (currently on `fly-serve-backend`): dispatch to
  the runner pool, per-worker RAM, base-stacking, the std-threading fix.
- GC of built images in the registry (same concern as today's worker images).
- Whether the runner needs any per-worker config (env, etc.) beyond `bin` — the
  trampoline assumes not; revisit if a worker needs custom image config.
