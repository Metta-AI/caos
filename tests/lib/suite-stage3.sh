#!/bin/bash
# Suite stage 3 (the `then` of the build tool): --result is the ARTIFACT
# TREE {report, bin/, images/{runner,bash,cargo}}; select the tests and
# map-then over them, with suite-summarize.sh as the `then`.
#
# The per-test jobs key on the CONTENT-STABLE inputs only — the bin tree,
# the image refs (registry digests — content addresses), the test's own
# tree, the harness script. Inputs that only one test needs ride in that
# test's map child as a wrapper tree — the pruned workspace for the tests
# that dogfood the tree (cargo-self, unit), chat-online's API key — so
# nobody else re-keys on them.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/result/images
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
LIB=/cas/args/workspace/tests/lib

# The test selection: every tests/<name> with a cli.sh — or just the names
# in --only (a filtered suite; its per-test jobs share their cache with full
# runs). Symlinks into the args materialize nothing — `caos put` resolves
# them to recorded hashes.
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
  case "$t" in
    cargo-self | unit)
      # Dogfood the tree under test — the PRUNED build tree (what cargo
      # reads, the compile's own input), so only Rust-relevant edits re-key
      # these, exactly like the compile itself.
      mkdir "/tmp/sel/$t"
      ln -s "/cas/args/workspace/tests/$t" "/tmp/sel/$t/test"
      ln -s /cas/args/build_ws "/tmp/sel/$t/workspace"
      ;;
    chat-online)
      mkdir /tmp/sel/chat-online
      ln -s "/cas/args/workspace/tests/$t" /tmp/sel/chat-online/test
      # The real-API key, when the suite was given one: same key, same cache
      # key — only this test re-keys when it rotates. Without one the test's
      # cli.sh self-skips.
      if [ -e /cas/args/api_key ]; then
        caos get /cas/args/api_key
        cp /cas/args/api_key /tmp/sel/chat-online/api_key
      fi
      ;;
    *) ln -s "/cas/args/workspace/tests/$t" "/tmp/sel/$t" ;;
  esac
done
caos put /tmp/sel /cas/sel

caos get /cas/args/workspace/crates
map=$(caos curry /cas/std/testenv -- \
  "--script:@=$LIB/run-nested.sh" \
  "--bins:@=/cas/args/result/bin" \
  "--worker_common:@=/cas/args/workspace/crates/worker-common" \
  "--runner_image:@=/cas/args/result/images/runner" \
  "--bash_image:@=/cas/args/result/images/bash" \
  "--cargo_image:@=/cas/args/result/images/cargo")
then_img=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-summarize.sh")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
