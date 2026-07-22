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
# runs). Each child is a uniform wrapper {test, bins/, images/, extras...}
# carrying ONLY what the test declared in its std-manifest (absent manifest =
# everything), so a worker edit re-keys the tests that use that worker, a
# toolchain change re-keys the cargo tests, and nothing else moves. Symlinks
# into the args materialize nothing — `caos put` resolves them to recorded
# hashes.
caos get /cas/args/result/bin
only=""
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  only=" $(cat /cas/args/only) "
fi

# Closure rules, manifest line -> ingredients:
#   bash | runner        that image
#   cargo                the cargo image + worker-cargo
#   rustc                worker-rustc + cargo's closure + the runner image
#   <name>               worker-<name> + the runner image (curry base)
#   bin:<name>           that binary (a test helper, e.g. llm-stub)
# The stack binaries (server, runnerd, caos-cli) ride always.
child() { # <test name>
  local t=$1 dir="/tmp/sel/$1"
  mkdir -p "$dir/bins" "$dir/images"
  ln -s "/cas/args/workspace/tests/$t" "$dir/test"
  for b in server runnerd caos-cli; do
    ln -s "/cas/args/result/bin/$b" "$dir/bins/$b"
  done
  local manifest="/cas/args/workspace/tests/$t/std-manifest"
  if [ ! -e "$manifest" ]; then
    # No manifest: everything (the safe default; tighten test by test).
    for b in /cas/args/result/bin/*; do
      ln -sf "$b" "$dir/bins/$(basename "$b")"
    done
    for i in runner bash cargo; do
      ln -s "/cas/args/result/images/$i" "$dir/images/$i"
    done
    return
  fi
  caos get "$manifest"
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    case "$entry" in
      bash) ln -sf /cas/args/result/images/bash "$dir/images/bash" ;;
      runner) ln -sf /cas/args/result/images/runner "$dir/images/runner" ;;
      cargo)
        ln -sf /cas/args/result/images/cargo "$dir/images/cargo"
        ln -sf /cas/args/result/bin/worker-cargo "$dir/bins/worker-cargo"
        ;;
      rustc)
        ln -sf /cas/args/result/bin/worker-rustc "$dir/bins/worker-rustc"
        ln -sf /cas/args/result/images/cargo "$dir/images/cargo"
        ln -sf /cas/args/result/bin/worker-cargo "$dir/bins/worker-cargo"
        ln -sf /cas/args/result/images/runner "$dir/images/runner"
        ;;
      bin:*) ln -sf "/cas/args/result/bin/${entry#bin:}" "$dir/bins/${entry#bin:}" ;;
      *)
        ln -sf "/cas/args/result/bin/worker-$entry" "$dir/bins/worker-$entry"
        ln -sf /cas/args/result/images/runner "$dir/images/runner"
        ;;
    esac
  done < "$manifest"
}

mkdir /tmp/sel
for d in /cas/args/workspace/tests/*/; do
  t=$(basename "$d")
  if [ -n "$only" ]; then
    case "$only" in *" $t "*) ;; *) continue ;; esac
  fi
  caos get "/cas/args/workspace/tests/$t"
  [ -e "/cas/args/workspace/tests/$t/cli.sh" ] || continue
  child "$t"
  case "$t" in
    cargo-self | unit)
      # Dogfood the tree under test — the PRUNED build tree (what cargo
      # reads, the compile's own input), so only Rust-relevant edits re-key
      # these, exactly like the compile itself.
      ln -s /cas/args/build_ws "/tmp/sel/$t/workspace"
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

caos get /cas/args/workspace/crates
map=$(caos curry /cas/std/testenv -- \
  "--script:@=$LIB/run-nested.sh" \
  "--worker_common:@=/cas/args/workspace/crates/worker-common")
then_img=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-summarize.sh")
caos map-then /cas/sel -- --map="$map" --then="$then_img"
