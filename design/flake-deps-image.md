# depsImage: incremental flake builds — design note

**Status:** implemented — the `build` stage carries the second memo, and
`std/cargo` is the first flake to expose `#depsImage`. Extends
`flake-images.md` (the image/worker/flake contract) — read that first.

## Problem

The build stage has exactly one memo: the registry tag `flake-<H>`, where `H`
is the flake tree's git hash. Any change to the tree — one source byte — is a
total miss, and the rebuild restarts from whatever `/nix/store` the
flake-builder image shipped. Nix's own derivation-level incrementality is
discarded between jobs, so the cost of a miss has no relationship to the size
of the change.

`std/cargo` is where this hurts. Its tree carries real source for `worker-cargo`
and `worker-common` (they build the `/worker`), so an edit to either re-keys the
tree and re-bakes the toolchain and all ~176 workspace dependencies — minutes,
for a change that touches two crates. Measured: a one-line edit to
`crates/worker-cargo/src/decompose.rs` moves `caosImage.drvPath` from
`zsdcml7y…` to `8yc8xi7g…`.

## The insight

The expensive half of a flake is usually **source-independent**, and nix already
knows it. For `std/cargo` the expensive half is `bake.deps` —
`craneLib.buildDepsOnly`, which crane routes through `mkDummySrc`: manifests,
lockfile, and empty stubs at each target path, with the real source content
discarded. Verified on the root flake: that derivation held at `yh16r476…`
across a `lib.rs` edit and across a `Cargo.toml` comment (crane normalizes
manifests before hashing), and moved only on `cargo add itoa`, to `ysyz14aj…`.

So a memo keyed on *that derivation* survives exactly the edits the tree hash
cannot.

## The contract

A flake MAY expose `packages.<system>.depsImage` alongside `caosImage`. It is
an image carrying the **build-time** store paths `caosImage`'s build consumes
but does not itself produce.

Three things it is not:

- **Not a runtime environment.** Nix skips a derivation iff its output path is
  *valid in the store*, so what belongs here is derivation outputs — the dep
  bake, the toolchain, the vendored sources — not what a person would want on
  `$PATH`.
- **Not runnable.** No entrypoint, no `/worker`, no caos, no setuid. The
  flake-builder unpacks it into its own store and never starts a container from
  it. Nothing about any runner or any caos version can enter its key — which is
  the property that keeps the memo good across caos builds.
- **Not `includeNixDB = true`.** That writes a `db.sqlite` which, unpacked over
  the consuming store, would *replace* the flake-builder's own registrations.
  The closure's registration rides as a plain file at
  **`/caos-deps-registration`** instead, and is merged with
  `nix-store --load-db`.

`pkgs.closureInfo { rootPaths = [ … ]; }` produces that registration, and
`buildLayeredImage` pulls the whole closure in behind it.

## The flow

The `build` stage becomes, unchanged except where noted:

1. `H = caos hash <tree>`; probe `flake-<H>`. Hit → return `{ref, config}`.
   **Unchanged.**
2. Miss → `nix eval path:/tmp/ws#depsImage.drvPath` → `D` (the store hash of
   the drv). No such output → build the whole image as before.
3. Probe `deps-<D>`.
   - **Hit** → `skopeo copy` it to an OCI layout, unpack each layer into `/`,
     `nix-store --load-db < /caos-deps-registration`.
   - **Miss** → `nix build path:/tmp/ws#depsImage` right here, push as
     `deps-<D>`. The paths are already in this container's store.
4. `nix build path:/tmp/ws#caosImage`. The closure is present either way, so
   only the delta builds.
5. Stream to `flake-<H>`, return. The `stack` stage is **unchanged**.

## Why this shape

**The cold path cannot regress.** A deps miss builds in the same container that
then builds the image, so there is no transfer at all — it is today's single
build with an extra push. The split only ever adds a fast path.

**The eval is free where it matters.** `nix eval` runs only on a `flake-<H>`
miss, and a `flake-<H>` miss is a full build. 1.7–1.9s (host, warm) against
minutes is noise, and a flake with no `depsImage` pays it once and falls
through.

**Both memos are registry tags.** Same pattern as `flake-<H>` and `bake-<H>`:
no new infrastructure, and the memo lives with the artifact, so a registry GC
cannot leave a key pointing at nothing. Redis was considered and rejected —
the flake-builder is a worker with no redis path, and redis is documented
best-effort while the registry tag is the durable, caos-independent memo.

**Unpack, don't run.** Making the deps image runnable would mean stacking the
caos additions into it, putting the caos binary in its key — the leak
`bake.sh` already works around ("The job's request additionally keys on the
nix-builder IMAGE (which embeds the freshly built caos)") and that
`build.sh` dodges for nixbuilder by deliberately using the host caos. Unpacking
sidesteps the question entirely.

## Costs and edges

- `drvPath` is input-addressed, so a nixpkgs or crane pin bump re-keys
  `deps-<D>` even when the closure would be byte-identical. Conservative in the
  safe direction; expect one rebake per pin bump.
- Divergence is safe. If `deps-<D>` does not cover everything `caosImage` needs,
  the build just builds the remainder — a miss, not corruption. So a flake can
  be liberal about what it puts in `rootPaths`.
  **Be liberal, then — an under-filled `rootPaths` is silent.** Nothing fails,
  nothing warns; the builder just quietly does the work again, and the only
  symptom is a slow tool. The root flake's list was
  `(cargoArtifacts, toolchain, flake-builder, runner)` for a year and read as
  complete. Measured against a warm `deps-<D>`, the builder still had to
  produce **809** store paths: 747 fetched from `cache.nixos.org` and **62
  built from source in the container**. Two of those dominated —
  `docker-29.5.3`, because `stackUserland` takes it with
  `.override { buildxSupport = false; composeSupport = false; }` and an
  overridden derivation is in no binary cache (a full Go compile, observed at
  300% CPU); and `caos-worker-cargo.tar.gz`, a 3.4 GB closure to tar and gzip,
  pulled in because the seed pushes the three clean core images. Naming
  `stackUserland`, `builtinWorkerImages` and the seed's own tools brought it to
  60 must-build paths, all of them cheap (hooks, `conf.json`, dummy manifests),
  and nix skips even those wherever the consuming output is already valid.
  **The check is mechanical**, so run it rather than reasoning about it:

  ```sh
  nix-store -qR --include-outputs $(nix eval --raw .#caosImage.drvPath) \
    | grep -v '\.drv$' | sort -u > need
  reg=$(nix-store -qR $(nix eval --raw .#depsImage.drvPath) \
    | grep 'deps-registration.*\.drv$')
  nix-store -qR $(nix-store -q --outputs "$reg") | sort -u > have
  comm -23 need have    # everything the builder still has to produce
  ```

  Note `nix path-info -r` on the deps **tarball** answers a different question
  (its scanned references) and reports far too little — go through the
  registration derivation, as above.
- Unpacking writes `/nix/store`, so it needs root. The flake-builder already
  runs with `CAOS_WORKER_UID=0`.
- `gnutar` joins the flake-builder image: `coreutils` has no `tar`, and the
  layers arrive as tarballs. GNU tar auto-detects the layer's compression.
- **`/var/tmp` must exist in the flake-builder image.** skopeo stages *pulled*
  blobs on disk there, and a minimal nix image has no `/var/tmp`, so a
  `deps-<D>` hit died on `creating temporary on-disk layer`. The push
  direction streams and never noticed, so only the hit path shows it. This
  containers/image build ignores `$TMPDIR` for that path (measured: exporting
  it changed nothing), so the worker makes the directory itself.

## Measured

On `std/cargo`, one comment appended to `crates/worker-cargo/src/decompose.rs`
(a real `flake-<H>` miss — the published tree hash moved), against a warm
`deps-<D>`:

| | |
|---|---|
| cold: flake miss, deps miss | **303s** |
| flake miss, deps hit | **140s** |

`depsImage.drvPath` held at `id4n9bvw…` across that edit while
`caosImage.drvPath` moved `yvjzsxwx…` → `flfs3jbw…` — the property the whole
note rests on, on the flake that motivated it.

## What it unlocks

The only reason `std/cargo`'s tree is curated — empty stubs for the crates that
do not build the `/worker`, `std/refresh.sh` to generate them, `tests/std-lint`
to keep them honest — is that the tree hash was the sole memo, so anything in
the tree was a rebuild trigger. With the expensive half keyed on a
source-independent derivation, a tree can carry full source and still hit. The
stub machinery becomes an optimization rather than a requirement.

## Adjacent, not done

A **warm flake-builder runner** would keep `/nix/store` hot in-process across
jobs, making even a `deps-<D>` hit free. Orthogonal to this note and composes
with it.

~~**Seeding the layer tarballs, not just the outputs.** `dockerTools` builds
one `.tar.gz` derivation per store path, and a fresh builder container has to
re-tar and gzip every layer of the ~3.4GB closure.~~ **Retracted (2026-07-29)
— that is not what the current `dockerTools` does.** `buildLayeredImage` is
`streamLayeredImage` piped into `pigz`: the per-store-path split lives in
`layers.json` (computed once by `auto-layer.py`) and the tarring happens
inside the *stream script*, so there is exactly ONE `.tar.gz` derivation per
image, not one per layer. Verified on the whole tree — a full image build is
six derivations (`customisation-layer`, `excludePaths`, `layers.json`,
`conf.json`, `stream-…`, `…tar.gz`). There is nothing per-layer to seed.

What was really left of that time is covered above: the `rootPaths` list was
incomplete, so a Go toolchain build and a 3.4 GB tar+gzip ran on every build.

**Still adjacent:** a **warm flake-builder runner** keeping `/nix/store` hot
across jobs would make even a `deps-<D>` hit free — the transplant
(`skopeo copy` + untar the closure into `/`) is now the largest fixed cost of
a build, and it exists only because each job starts from a cold store.

**Two cold flake evaluations — tried collapsing, inconclusive.** The stage runs
`nix eval …#depsImage.drvPath` and then `nix build …#caosImage`, and a `path:`
flake gets no eval cache, so this tree (which instantiates nixpkgs twice, for
host and Linux) is evaluated from scratch twice; the first samples at ~18s
in-container. The obvious fix — one `builtins.getFlake` + `--impure` eval
returning both `drvPath`s, then `nix build <drv>^out` so nix never re-enters
the evaluator — was implemented and reverted. It is *correct* (identical
drvPath either way, verified); it simply did not measurably help.

**And that is the more useful finding: this workload's run-to-run spread is
~45s.** Two runs of the *identical* tree and builder gave 2m20 and 3m04; the
single-eval variant gave 2m53, i.e. inside that band. A one-run A/B here
proves nothing, and the numbers in this note that come from single runs
(including the table above) should be read with that error bar. Structural
evidence — "this derivation is no longer built at all", checked with the
`comm -23 need have` recipe above — is worth more than a stopwatch, and is how
the `rootPaths` fix was actually confirmed.
