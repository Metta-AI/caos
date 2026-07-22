#!/bin/bash
# Suite stage 2 (the `then` of the workspace build): select the test dirs,
# curry the per-test image, map-then over the tests, summarize.
#
# The per-test jobs key on the CONTENT-STABLE inputs only — the bin tree
# (never the whole cargo result: its stderr carries volatile timings), the
# test's own tree, the harness script, the image IDs. Inputs that only one
# test needs ride in that test's map child as a wrapper tree — cargo-self's
# workspace, chat-online's API key — so nobody else re-keys on them.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/result/exit
if [ "$(cat /cas/args/result/exit)" != 0 ]; then
  caos get /cas/args/result/stderr || true
  tail -60 /cas/args/result/stderr >&2 || true
  echo "SUITE: workspace build failed" >&2
  exit 1
fi

# The test selection: every tests/<name> with a cli.sh. Symlinks into the
# args materialize nothing — `caos put` resolves them to recorded hashes.
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
mkdir /tmp/sel
for d in /cas/args/workspace/tests/*/; do
  t=$(basename "$d")
  caos get "/cas/args/workspace/tests/$t"
  [ -e "/cas/args/workspace/tests/$t/cli.sh" ] || continue
  case "$t" in
    cargo-self)
      # Dogfoods the tree under test: only ITS child carries the workspace,
      # so source edits re-key cargo-self alone (plus the build, of course).
      mkdir /tmp/sel/cargo-self
      ln -s "/cas/args/workspace/tests/$t" /tmp/sel/cargo-self/test
      ln -s /cas/args/workspace /tmp/sel/cargo-self/workspace
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
  "--bins:@=/cas/args/result/bin" \
  "--worker_common:@=/cas/args/workspace/crates/worker-common" \
  "--runner_image:@=/cas/args/runner_image" \
  "--bash_image:@=/cas/args/bash_image" \
  "--cargo_image:@=/cas/args/cargo_image")
then_img=$(caos curry /cas/std/bash -- "--script:@=/cas/args/summarize")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
