#!/bin/bash
# Suite stage 3 (the `then` of the image builds): --children holds the
# runner/bash image digest refs; select the tests and map-then over them.
#
# The per-test jobs key on the CONTENT-STABLE inputs only — the bin tree
# (never the whole cargo result: its stderr carries volatile timings), the
# image refs (registry digests — content addresses), the test's own tree,
# the harness script. Inputs that only one test needs ride in that test's
# map child as a wrapper tree — cargo-self's workspace, chat-online's API
# key — so nobody else re-keys on them.
set -euo pipefail

caos get /cas/args/children
caos get /cas/args/build

# The test selection: every tests/<name> with a cli.sh — or just the names
# in --only (a filtered suite; its per-test jobs share their cache with full
# runs). Symlinks into the args materialize nothing — `caos put` resolves
# them to recorded hashes.
only=""
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  only=" $(cat /cas/args/only) "
fi
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
mkdir /tmp/sel
for d in /cas/args/workspace/tests/*/; do
  t=$(basename "$d")
  if [ -n "$only" ]; then
    case "$only" in *" $t "*) ;; *) continue ;; esac
  fi
  caos get "/cas/args/workspace/tests/$t"
  [ -e "/cas/args/workspace/tests/$t/cli.sh" ] || continue
  case "$t" in
    cargo-self)
      # Dogfoods the tree under test — the PRUNED build tree (what cargo
      # reads, threaded through as the build's input), so only Rust-relevant
      # edits re-key cargo-self, exactly like the build itself.
      mkdir /tmp/sel/cargo-self
      ln -s "/cas/args/workspace/tests/$t" /tmp/sel/cargo-self/test
      ln -s /cas/args/build_ws /tmp/sel/cargo-self/workspace
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
  "--script:@=/cas/args/run_nested" \
  "--bins:@=/cas/args/build/bin" \
  "--worker_common:@=/cas/args/workspace/crates/worker-common" \
  "--runner_image:@=/cas/args/children/runner" \
  "--bash_image:@=/cas/args/children/bash" \
  "--cargo_image:@=/cas/args/cargo_image")
then_img=$(caos curry /cas/std/bash -- "--script:@=/cas/args/summarize")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
