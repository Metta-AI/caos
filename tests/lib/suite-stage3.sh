#!/bin/bash
# Suite stage 3 (the `then` of the build tool): --result is THE TEST STACK
# IMAGE (design/test-stack-image.md). Select the tests, then map-then over
# them with that image as the map worker — one stack per test, each carrying
# the binaries and the std trees, so nothing has to be handed in.
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

# The map worker IS the built image, curried with the per-test runner. Its
# /worker brings the inner stack up and runs that script against it.
map=$(caos curry /cas/args/result -- "--worker1:@=$LIB/run-test.sh")
then_img=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/suite-summarize.sh")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
