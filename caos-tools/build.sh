#!/usr/bin/env bash
#@doc Build the test stack image from the tree. The flake-builder builds only
#@doc the BUILD ENVIRONMENT (#caosImage) from a REDUCED tree; this tool then
#@doc compiles the live sources in it and assembles the stack image.
#
# THREE STAGES, one script, selected by a curried --stage (the std/flake-builder
# pattern).
#
#   reduce   (default) cut the tree down to what determines the ENVIRONMENT and
#            hand THAT to the flake-builder
#   launch   the builder image's ref is only knowable once the flake-builder has
#            run, so currying the compile onto it needs its own stage
#   make     runs IN the builder image: compile the passed-in source, construct
#            std, assemble the stack image
#
# WHY THE REDUCTION. `#caosImage` used to BE the test stack, built from sources
# inside the flake tree — so every source edit moved the tree hash, missed
# `flake-<H>`, and re-ran nix from a cold store: a 196 MiB flake-input fetch, a
# 2.1 GiB deps transplant, ~42s of crane-hook derivations. `#depsImage` existed
# only to paper over that. Take the sources out of the flake's input and the
# hash holds still, so the flake-builder is a registry hit on every source edit
# and `#depsImage` has nothing left to carry — it is deleted.
#
# Verified: the reduced tree yields a BYTE-IDENTICAL `#caosImage` derivation to
# the full tree, from 59 files / 240 KB instead of 164 files.
set -euo pipefail

fail() { echo "BUILD FAIL: $*" >&2; exit 1; }

# Args are lazy placeholders — fetch before reading. The initial invocation
# carries no --stage, so the fetch fails and we default to the reduction.
stage=reduce
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

case "$stage" in

reduce)
  caos get -r /cas/args/in
  [ -e /cas/args/in/flake.nix ] || fail "no flake.nix in --in; this tool builds the caos tree"

  # What determines the ENVIRONMENT, and nothing that determines the program:
  #
  #   *.nix, */flake.lock, rust-toolchain.toml    the environment
  #   Cargo.toml, Cargo.lock, crates/*/Cargo.toml the dependency graph
  #   std/**                                      the flake installs
  #                                               std/bash/worker as the builder
  #                                               image's /worker and reads
  #                                               std/cargo/bake.nix
  #   a ZERO-BYTE file at every .rs path          the target SET
  #
  # The zero-byte .rs files are load-bearing: cargo discovers and validates
  # targets by file PRESENCE — `[[bin]] path = "src/bin/caos.rs"` with no such
  # file is a hard error — so keeping the paths and dropping the bytes encodes
  # exactly "which targets exist" and nothing about what they do. Deliberately
  # NOT parsing manifests to find target paths: mirroring cargo's discovery
  # rules would drift, and every .rs path is a superset that cannot miss one.
  #
  # MODES ARE PRESERVED for std/: a 644 copy of the 755 std/bash/worker is a
  # different store path and so a different image derivation (measured).
  #
  # Both ways of getting this wrong are SAFE. Too little: evaluation fails
  # outright ("path .../std/bash/worker does not exist"). Wrong modes: a
  # different derivation. Neither yields a stale build behind a correct-looking
  # cache hit — which is exactly what `#depsImage`'s rootPaths could do.
  R=/tmp/reduced
  rm -rf "$R"; mkdir -p "$R"
  cd /cas/args/in
  for f in rust-toolchain.toml Cargo.toml Cargo.lock; do
    if [ -f "$f" ]; then install -D -m 644 "$f" "$R/$f"; fi
  done
  find . \( -name '*.nix' -o -name 'flake.lock' \) -print0 \
    | while IFS= read -r -d '' f; do install -D -m 644 "$f" "$R/$f"; done
  if [ -d std ]; then
    find std -type f -print0 \
      | while IFS= read -r -d '' f; do mkdir -p "$R/$(dirname "$f")" && cp -p "$f" "$R/$f"; done
  fi
  find crates -name Cargo.toml -print0 2>/dev/null \
    | while IFS= read -r -d '' f; do install -D -m 644 "$f" "$R/$f"; done
  find crates -name '*.rs' -print0 2>/dev/null \
    | while IFS= read -r -d '' f; do mkdir -p "$R/$(dirname "$f")"; : > "$R/$f"; done

  # A worker mints a tree by putting it at a FRESH DIRECT CHILD of /cas
  # (validate_target) — it cannot ingest an arbitrary host path.
  caos put "$R" /cas/reduced

  # THE BUILD'S OWN INPUTS, and nothing else. `make` compiles the workspace,
  # runs build-builtins.sh (which reads std/ and crates/worker-common), and
  # installs stack/serve and test-stack/worker into the image. It reads
  # nothing else in the tree.
  #
  # It used to get the WHOLE tree, so a one-line edit to tests/<name>/cli.sh —
  # or to a design doc, or this comment — recompiled the workspace,
  # republished std and reassembled the 218 MB image before running the test
  # that changed: 9.7s of a 16.9s single-test run, traced. Which is also why a
  # test edit re-keyed every OTHER test: the image digest is in every per-test
  # key.
  #
  # MODES ARE PRESERVED, for the same reason the reduction preserves them: a
  # 644 copy of the 755 std/bash/worker is a different image derivation.
  #
  # Same safety argument as the reduction above, and it is why this is worth
  # doing by hand: prune something `make` needs and it FAILS on a missing
  # path. There is no way to get a stale image out of a too-small src.
  S=/tmp/src
  rm -rf "$S"; mkdir -p "$S"
  for e in Cargo.toml Cargo.lock rust-toolchain.toml \
           crates std stack test-stack build-builtins.sh; do
    if [ -e "$e" ]; then cp -RL --preserve=mode "$e" "$S/$e"; fi
  done
  caos put "$S" /cas/src

  # The build source rides into `launch` as a curried arg so `make` can reach
  # it: run-then passes only --in (here, the reduced tree) and --result.
  launch=$(caos curry /cas/std/bash -- \
    "--worker1:@=/cas/args/worker1" --stage=launch "--src:@=/cas/src") \
    || fail "currying the launch stage"

  caos run-then /cas/reduced -- --run=/cas/std/flake-builder --then="$launch"
  ;;

launch)
  # /cas/args/result is the flake-builder's result: the BUILD ENVIRONMENT image.
  # Curry `make` onto it — `caos curry` takes the ref as a runtime string, so
  # currying onto an image that did not exist when this was written is fine.
  caos get -r /cas/args/result
  image=$(caos hash /cas/args/result) || fail "reading the builder image ref"

  # The builder's own delta tree rides in as DATA too (`--builder`), not just as
  # the image to run in. `make` extends it: the assembled stack image is that
  # same {base, config.json, layer00} with one more layer on top. It cannot
  # simply name the builder as its `base`, because `base` must be a
  # `docker://<ref>` blob and convert_git_image has no git-tree-base case —
  # chaining caos images by hash is exactly the extension we discussed and do
  # not have yet.
  make=$(caos curry "$image" -- \
    "--worker1:@=/cas/args/worker1" --stage=make \
    "--src:@=/cas/args/src" "--builder:@=/cas/args/result") \
    || fail "currying the make stage onto the builder image"

  # --in is immaterial to `make` (everything it needs is curried in), but
  # run-then requires one; the reduced tree is already in hand.
  caos run-then /cas/args/in -- --run="$make"
  ;;

make)
  # Runs IN the builder image: pinned toolchain on PATH, the dependency
  # `target/` already inflated as real files at the path the bake recorded, and
  # the stack userland for the image this assembles.
  #
  # A PHASE CLOCK, because this stage is the whole cost of a warm `run-tool
  # build` and the compile is the small half of it. Without these lines the
  # only observable is one opaque container lifetime, and the two expensive
  # phases (git-writing 167 MB of binaries, then hashing them again into the
  # result) look exactly like a slow compile.
  ts() { echo "build: [${SECONDS}s] $*" >&2; }
  caos get -r /cas/args/src
  caos get -r /cas/args/builder
  ts "fetched src + builder"

  # ---- 1. compile ----------------------------------------------------------
  # AT THE RECORDED PATH. The bake fingerprints on the absolute workspace root
  # and target dir — build-script executables and their OUT_DIRs are keyed on
  # the target dir too — so materializing anywhere else silently rebuilds all
  # ~176 deps (12.6s of a 15.0s cold build). Right, it is 2.4s.
  wsroot=$(cat /ws-root) || fail "no /ws-root in the builder image"
  targetdir=$(cat /target-dir) || fail "no /target-dir in the builder image"
  mkdir -p "$wsroot"
  cp -rL /cas/args/src/. "$wsroot/"
  cd "$wsroot"
  export CARGO_TARGET_DIR="$targetdir"
  CARGO_BUILD_TARGET="$(uname -m)-unknown-linux-musl"
  export CARGO_BUILD_TARGET
  # The TEST world (crates/caos-world, read via option_env! at compile time).
  # What the flake used to stamp on testWorkspaceBins, and it is load-bearing:
  # without it these binaries are host-world, a host client is accepted by this
  # stack instead of refused, and tests/world-guard sees a 404 on its bogus
  # object id where it wanted a 400. Only crates/caos-world reads it, so the
  # dependency bake is unaffected.
  export CAOS_WORLD=test
  # The other half of the bake's contract, and it is not optional: bake.env
  # points CARGO_HOME at a writable /tmp/cargo and leaves CAOS_VENDOR_CONFIG
  # for the worker to install there ("a writable home; the worker copies the
  # vendor config here"). Without it cargo resolves from crates.io rather than
  # the vendored sources, every dependency re-fingerprints, and all ~176
  # rebuild — silently, which is exactly what the guard below caught.
  mkdir -p "${CARGO_HOME:?bake.env should set CARGO_HOME}"
  cp "${CAOS_VENDOR_CONFIG:?bake.env should set CAOS_VENDOR_CONFIG}" \
     "$CARGO_HOME/config.toml"
  cargo build --workspace --locked 2>&1 | tee /tmp/compile.log >&2 \
    || fail "cargo build"

  # A STANDING GUARD, not a report. If the workspace does not materialize at the
  # exact path the bake fingerprinted — `pwd -P` at bake time, and the same
  # target dir, because build-script executables and their OUT_DIRs are keyed on
  # it too — cargo silently rebuilds all ~176 dependencies. Measured on the
  # host: 12.6s of a 15.0s cold build, against 2.4s for the workspace alone.
  # There is no error, just a slow build, which is precisely the kind of thing
  # that goes unnoticed for a year. So assert it instead: this only ever
  # compiles the workspace, because a lockfile or toolchain change moves the
  # REDUCED tree and rebuilds the builder image (and its bake) first.
  n=$(grep -c '^ *Compiling' /tmp/compile.log || true)
  ts "compiled $n crates"
  [ "$n" -le 24 ] || fail "compiled $n crates — the dependency bake was NOT reused.
  The workspace must materialize at \$(cat /ws-root)=$wsroot with
  CARGO_TARGET_DIR=\$(cat /target-dir)=$targetdir; one of those no longer
  matches what the bake recorded."
  BIN=$targetdir/$CARGO_BUILD_TARGET/debug
  [ -x "$BIN/caos-cli" ] || fail "no caos-cli at $BIN after the build"

  # build-builtins.sh resolves every binary as `<path>/bin/<name>` — it was
  # written against nix store paths, which have a bin/ subdirectory. A cargo
  # target dir does not, so hand it a staged nix-shaped layout rather than the
  # target dir itself. (Measured the hard way: "build-builtins: no binary caos
  # in the built paths".) CAOS_STACK_BIN below is NOT this shape — `serve`
  # wants `$CAOS_STACK_BIN/server` directly.
  STAGED=/tmp/staged-bins
  rm -rf "$STAGED"; mkdir -p "$STAGED/bin"
  for b in "$BIN"/caos "$BIN"/caos-cli "$BIN"/server "$BIN"/runnerd \
           "$BIN"/worker-*; do
    [ -x "$b" ] && [ -f "$b" ] && install -m 755 "$b" "$STAGED/bin/$(basename "$b")"
  done
  [ -x "$STAGED/bin/caos" ] || fail "no caos binary staged from $BIN"
  ts "staged binaries"

  # ---- 2. publish std ------------------------------------------------------
  # Reuses build-builtins.sh against a two-member stack (redis + server; no
  # runnerd — publishing dispatches no jobs), exactly as the retired seed
  # derivation did. The design end-state is pure git plumbing with no server at
  # all — every hash is known locally once bases are pinned by digest — but
  # that means hand-rolling curry objects byte-identically to `curry_object`,
  # and this way reuses tested code. It runs ONCE per tree change, not once per
  # test, so the bring-up is amortised across a whole suite run.
  state=/tmp/seed-stack
  rm -rf "$state"; mkdir -p "$state"
  CAOS_STACK_STATE=$state \
  CAOS_STACK_LOGS=/tmp/seed-logs \
  CAOS_STACK_READY=/tmp/seed-ready \
  CAOS_STACK_BIN="$BIN" \
  CAOS_STACK_REDIS_PORT=6391 \
  CAOS_STACK_REDIS_PERSIST=no \
  CAOS_STACK_REGISTRY=no \
  CAOS_STACK_RUNNERD=no \
    bash "$wsroot/stack/serve" > /tmp/seed-serve.log 2>&1 &
  serve=$!
  for _ in $(seq 1 60); do
    [ -e /tmp/seed-ready ] && break
    kill -0 "$serve" 2>/dev/null || { cat /tmp/seed-serve.log >&2; fail "seed stack died"; }
    sleep 1
  done
  [ -e /tmp/seed-ready ] || { cat /tmp/seed-serve.log >&2; fail "seed stack never came up"; }
  ts "seed stack up"

  # The registry is the OUTER stack's, reached by the name that resolves on
  # caos-net — which is also the name the inner server will pull a delta's
  # `base` with. The clean core images push under a store-path-keyed tag, so
  # these are registry hits whenever the host got there first.
  CAOS_SERVER_URL=http://127.0.0.1 \
  CAOS_CLI="$BIN/caos-cli" \
  CAOS_CLIENT_REPO=/tmp/seed-client \
  CAOS_REGISTRY_HTTP=caos-registry:5000 \
  CAOS_BUILTIN_IMAGES="$(echo /caos/images/*.tar.gz)" \
  CAOS_BUILTIN_BINS="$STAGED" \
    bash "$wsroot/build-builtins.sh" >&2 || fail "publishing std"

  # The std tree the seed stack just published, as a hash.
  STD=$(git -C "$state/git" rev-parse refs/caos/std) || fail "no refs/caos/std in the seed repo"
  # The seed records (design/caos-expr.md, Phase 3), if bootstrap published any:
  # flake-builder resolves via `run docker://seeded`, answered by the inner
  # core-seeder-runner from these. Optional so a std without seeded core still
  # builds.
  SEED=$(git -C "$state/git" rev-parse refs/caos/seed 2>/dev/null || true)

  # Down before the repo is read: the server holds it open, and a half-written
  # pack is not something to hand on.
  kill "$serve" 2>/dev/null || true
  wait "$serve" 2>/dev/null || true
  ts "published std"

  # std LEAVES this stage as a value, not baked into the image, so a caller can
  # hand each job only the entries it needs (caos-tools/test.sh, stage3).
  #
  # It was published into the seed stack's repo, so move it the way values move:
  # push the closure to the outer server (a bare tree to a ref, as
  # GitTransport::ensure_pushed does), then symlink a PLACEHOLDER of it into the
  # result. `caos put` resolves a symlink into /cas to its recorded hash, so the
  # tree rides on by identity and nothing is materialized.
  git -C "$state/git" push --quiet "$CAOS_SERVER_URL" "$STD:refs/caos/std-built-$STD" \
    || fail "pushing the published std to the outer server"
  caos get-hash "$STD" /cas/std-built || fail "materializing the std placeholder"
  ts "handed std over ($STD)"

  # The seed records ride over the same way — one small tree whose closure
  # carries the flake-builder delta the seeder returns. Each test stack fetches
  # it and its core-seeder-runner answers from it (test-stack/worker).
  if [ -n "$SEED" ]; then
    git -C "$state/git" push --quiet "$CAOS_SERVER_URL" "$SEED:refs/caos/seed-built-$SEED" \
      || fail "pushing the published seed to the outer server"
    caos get-hash "$SEED" /cas/seed-built || fail "materializing the seed placeholder"
    ts "handed seed over ($SEED)"
  fi

  # ---- 3. assemble ---------------------------------------------------------
  # {image, std, bin} — the three things a caller needs, separately, so that
  # what re-keys a job is what that job actually uses. The image holds only what
  # bringing a stack up requires; the worker binaries are in `bin`; std is the
  # value above. A one-line edit to a leaf worker leaves the IMAGE untouched
  # (measured: 1 of 11 binaries changes, and the four in the image are not it).
  OUT=/tmp/out
  rm -rf "$OUT"; mkdir -p "$OUT"
  ln -s /cas/std-built "$OUT/std"
  # The seed records, when bootstrap published them (a symlink put, recorded-hash
  # reuse — no bytes move). Consumed by caos-tools/test.sh stage3 → each test
  # wrapper → test-stack/worker, which seeds refs/caos/seed into the inner stack.
  if [ -n "$SEED" ]; then ln -s /cas/seed-built "$OUT/seed"; fi

  IMG=$OUT/image
  mkdir -p "$IMG"
  cp /cas/args/builder/base "$IMG/base"
  cp /cas/args/builder/config.json "$IMG/config.json"
  if [ -d /cas/args/builder/layer00 ]; then cp -RL /cas/args/builder/layer00 "$IMG/layer00"; fi

  # The binaries a STACK needs to come up, and nothing else: `serve` starts
  # server and runnerd, and the image's caos is the tested one every inner
  # image carries. The worker binaries used to ride here too, which is what
  # made every worker edit move the image and re-key all twenty tests.
  L=$IMG/layer01
  mkdir -p "$L/caos/bin" "$L/caos/stack"
  for b in caos caos-cli server runnerd core-seeder-runner; do
    install -m 755 "$BIN/$b" "$L/caos/bin/$b" || fail "no binary $b"
  done

  # NO `bin` OUTPUT. It carried four host binaries for tests to copy out of
  # CAOS_BIN_DIR, and exactly one was ever named by a test: llm-stub. That is a
  # std entry now (std/llm-stub, built by cargo — it is a plain sidecar process,
  # not a worker image), so a test reaches it the way it reaches everything
  # else: a DEPS ref, mounted by deep-deps. The other three were named by
  # nothing at all.
  install -m 755 "$wsroot/stack/serve" "$L/caos/stack/serve"
  install -m 755 "$wsroot/test-stack/worker" "$L/worker"
  # NO /caos/seed-git. std arrives as an arg now and the stack's /worker seeds
  # from it, so which entries a stack has is the CALLER's choice — which is the
  # entire point: a test that never touches std/rgrep no longer re-keys when
  # rgrep changes.
  ts "assembled the result tree"

  # This stage's elapsed seconds, as a FOURTH output. Callers that want to
  # report it read it; the per-test wrappers never carry it, so a build whose
  # image/std/bin come out identical but took a different time re-keys stage3
  # and the summariser (one cheap container each) and leaves every test a hit.
  #
  # Only this stage's own time — reduce and launch are ~2s of container churn
  # before it, and the client's own `time` covers the whole tool. A START time
  # cannot be curried forward to total them up: args are the cache key, so a
  # timestamp in `make`'s args would mean `make` never caches again.
  #
  # Replayed on a cache hit, like every other duration in a result: it says
  # what this build cost when it last actually ran.
  echo "$SECONDS" > "$OUT/time"

  caos put "$OUT" /cas/out
  ts "put the result tree"
  ;;

*)
  fail "unknown --stage: $stage"
  ;;
esac
