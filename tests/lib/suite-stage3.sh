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
map=$(caos curry /cas/args/result -- "--worker1:@=$LIB/run-test.sh")

# The test selection: every tests/<name> with a cli.sh — or just the names in
# --only (a filtered suite; its per-test jobs share their cache with full
# runs). Each child is a wrapper {test, workspace?, api_key?} carrying only
# what that test needs beyond the image. Symlinks into the args materialize
# nothing — `caos put` resolves them to recorded hashes.
only=""
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  only=" $(cat /cas/args/only) "
fi

# --test_salt rides in EVERY per-test wrapper and nowhere else, so a fresh
# value re-runs all the tests and leaves the build a cache hit. Nothing reads
# the file: its presence in the wrapper is what moves the per-test key. Do not
# "clean up" the unused write — it is the whole mechanism.
salt=""
if [ -e /cas/args/test_salt ]; then
  caos get /cas/args/test_salt
  salt=$(cat /cas/args/test_salt)
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
  case "$t" in
    cargo-self | unit)
      # Dogfood the tree under test — the PRUNED build tree (what cargo
      # reads, the compile's own input), so only Rust-relevant edits re-key
      # these, exactly like the compile itself.
      ln -s /cas/args/build_ws "/tmp/sel/$t/workspace"
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
      if [ -e /cas/args/api_key ]; then
        caos get /cas/args/api_key
        cp /cas/args/api_key /tmp/sel/chat-online/api_key
      fi
      ;;
  esac
done
caos put /tmp/sel /cas/sel

then_img=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/suite-summarize.sh")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
