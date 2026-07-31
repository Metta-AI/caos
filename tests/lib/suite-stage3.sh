#!/bin/bash
# Suite stage 3 (the `then` of the build tool): --result is THE TEST STACK
# IMAGE (design/test-stack-image.md). Curry the per-test runner onto it,
# select the tests, and map-then over them — one stack per test, each
# carrying the binaries and a SEEDED std, so nothing has to be handed in.
#
# There used to be a stage between this and the build: a single job that
# published std into the host registry before the fan-out, because nineteen
# stacks starting on a cold registry all missed the same memo and all baked
# the toolchain, filling the outer pool with 20-minute jobs until whatever was
# still queued died on the pending timeout (`no runner for req (waited 900s)`,
# measured). That whole stage is gone: std is published ONCE, when the image
# is built (design/one-stack-image.md, "The seed"), so there is no cold-start
# herd left to serialize.
#
# Per-test jobs key on (the image digest, the test's own tree, the runner
# script). A source edit moves the image and re-keys every test, which is
# what a binaries change already did. The std-manifest closure rules are
# gone with the wrapper: there is no longer a per-test choice of which
# binaries and images ride along, because they all ride in the one image.
set -euo pipefail

caos get /cas/args/result
# One level further: the per-test subsets below symlink to individual entries,
# so `std` and `bin` must exist as placeholders for `caos put` to resolve them
# by recorded hash. Placeholders only — no content is fetched here.
caos get /cas/args/result/std
caos get /cas/args/result/bin
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
LIB=/cas/args/workspace/tests/lib

# The per-test map worker is curried HERE, where the image is a genuine
# --result tree. Passing the image itself as a curried arg to a later stage
# does not work: `caos curry /cas/args/<argname>` curries over the arg NODE,
# so the resulting worker inherits this job's own bindings (observed: a map
# job whose args were `image in worker1`, running as uid 1000 because the
# image's root grant was not in play either).
map=$(caos curry /cas/args/result/image -- "--worker1:@=$LIB/run-test.sh")

# The test selection: every tests/<name> with a cli.sh — or just the names in
# --only (a filtered suite; its per-test jobs share their cache with full
# runs). Each child is a wrapper {test, workspace?, api-key?} carrying only
# what that test needs beyond the image. Symlinks into the args materialize
# nothing — `caos put` resolves them to recorded hashes.
only=""
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  only=" $(cat /cas/args/only) "
fi

# --test-salt rides in EVERY per-test wrapper and nowhere else, so a fresh
# value re-runs all the tests and leaves the build a cache hit. Nothing reads
# the file: its presence in the wrapper is what moves the per-test key. Do not
# "clean up" the unused write — it is the whole mechanism.
salt=""
if [ -e /cas/args/test-salt ]; then
  caos get /cas/args/test-salt
  salt=$(cat /cas/args/test-salt)
fi

mkdir /tmp/sel
for d in /cas/args/workspace/tests/*/; do
  t=$(basename "$d")
  if [ -n "$only" ]; then
    case "$only" in *" $t "*) ;; *) continue ;; esac
  fi
  caos get "/cas/args/workspace/tests/$t"
  [ -e "/cas/args/workspace/tests/$t/cli.sh" ] || continue
  mkdir -p "/tmp/sel/$t"
  ln -s "/cas/args/workspace/tests/$t" "/tmp/sel/$t/test"
  if [ -n "$salt" ]; then printf '%s' "$salt" > "/tmp/sel/$t/salt"; fi

  # WHAT THIS TEST REACHES FOR, and nothing else. `uses-std` names the
  # /cas/std entries its jobs resolve; `uses-bin` the binaries it copies out
  # of CAOS_BIN_DIR to build its own curries. Each becomes a subtree of
  # symlinks into the build result, so the wrapper carries those entries BY
  # HASH — `caos put` resolves a symlink into /cas to its recorded hash, so
  # not one byte moves here.
  #
  # This is the whole mechanism. std used to be baked into the image, so any
  # worker binary moved the image and re-keyed all twenty tests; now a test's
  # key holds what it named. A worker-rgrep edit moves std/rgrep and
  # bin/worker-rgrep, which two tests name — the other eighteen are hits.
  #
  # Undeclared is UNAVAILABLE, deliberately: an unnamed std entry will not
  # resolve and an unnamed binary will not copy. Both fail loudly inside the
  # test, where a wrong declaration can only cost a red run, never a stale
  # green one. It cost four rounds of red to get these lists right, and every
  # one was the same rule:
  #
  #   THE GIT CLOSURE COVERS TREE REFERENCES AND NOTHING ELSE.
  #
  # Fetching a std entry brings its subtrees, so a curry's `args` ride along.
  # Three things do NOT, and each has to be named explicitly:
  #
  #   - A CURRY'S BASE. `std/rgrep/base` is a BLOB holding the base image's
  #     hash, not a reference to it, so declaring `rgrep` without `runner`
  #     yields "object not found" on the runner tree.
  #   - A HASH BOUND AS A LITERAL. `std/rustc` binds `--cargo=<hash>` the same
  #     way, so rustc needs `cargo`.
  #   - A NAME THE SERVER LOOKS UP. `std/bash` is a flake tree, and running one
  #     makes the server resolve `flake-builder` BY NAME — "std library has no
  #     flake-builder".
  #
  # And grep the CLIENT too, not just the test: `caos-cli talk` resolves
  # runner/bash-tool/llm-step/rgrep/bash from constants in chat.rs, and a Rust
  # worker builds its path with `std_image("bash")` (tests/commit) — neither
  # shows up in a search for /cas/std in the test directory.
  mkdir -p "/tmp/sel/$t/std" "/tmp/sel/$t/bin"
  if [ -e "$d/uses-std" ]; then
    caos get "/cas/args/workspace/tests/$t/uses-std"
    for e in $(cat "$d/uses-std"); do
      ln -s "/cas/args/result/std/$e" "/tmp/sel/$t/std/$e"
    done
  fi
  if [ -e "$d/uses-bin" ]; then
    caos get "/cas/args/workspace/tests/$t/uses-bin"
    for e in $(cat "$d/uses-bin"); do
      ln -s "/cas/args/result/bin/$e" "/tmp/sel/$t/bin/$e"
    done
  fi

  case "$t" in
    cargo-self | unit)
      # Dogfood the tree under test — the PRUNED build tree (what cargo
      # reads, the compile's own input), so only Rust-relevant edits re-key
      # these, exactly like the compile itself.
      ln -s /cas/args/build-ws "/tmp/sel/$t/workspace"
      ;;
    std-lint)
      # The literal-tree lints check the checked-in std copies against their
      # sources of truth ACROSS the tree, so this test gets the whole
      # workspace and honestly re-keys on any edit to it. It is a fast lint,
      # so that trade is fine.
      ln -s /cas/args/workspace "/tmp/sel/$t/workspace"
      ;;
    chat-online)
      # The real-API key, when the suite was given one: same key, same cache
      # key — only this test re-keys when it rotates. Without one the test's
      # cli.sh self-skips.
      if [ -e /cas/args/api-key ]; then
        caos get /cas/args/api-key
        cp /cas/args/api-key /tmp/sel/chat-online/api-key
      fi
      ;;
  esac
done
caos put /tmp/sel /cas/sel

# The build's own elapsed seconds ride into the summariser so the report can
# show them. Curried HERE because the summariser is the `then` of the map — it
# receives --children and nothing else.
#
# --start-time is the clock for the test phase, and it is taken HERE, one line
# before the fan-out fires, because this is the last point that certainly runs
# when the tests might. The summariser subtracts it from its own `now`.
#
# The phase cannot be recovered from the tests themselves: a test's start and
# end are files in its RESULT, so a cache hit replays the pair from whenever it
# last ran, and min/max across twenty records then spans back to that run (2306s
# against a 38s invocation, seen). Measured across two jobs that really ran, the
# number is right in both directions — a fan-out of cache hits is genuinely
# quick, and says so.
#
# A timestamp in args means the summariser never caches. That is the point: it
# is one cheap container, and it only runs at all when this stage does.
then_img=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/suite-summarize.sh" \
  "--build-time:@=/cas/args/result/time" "--start-time=$(date +%s)")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
