#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Proves the rustc builder loop: a Rust source file -> the builder compiles it
# (glibc/gnu, linking the vendored worker-common) and emits a ready-to-run worker
# = curry(runner, bin=<compiled binary>) -> that runs as an ordinary worker in the
# shared runner. Then it edits the source and rebuilds to confirm a distinct,
# independently-working worker.
#
# A STAGED TEST (dev/run-test/run-test.sh's header): four runs, five stages, no
# blocking. Two things that were free in one process are not free across five:
#
#   THE SALT MUST BE CARRIED, NOT REGENERATED. This test salts its sources with
#   a per-run marker so every run compiles a NOVEL binary and `first-run` is a
#   genuine cold path. cli.sh runs ONCE PER STAGE, so a `date +%s%N` at the top
#   would mint a different marker each time and stage 3 would rebuild rather
#   than reuse stage 1's image. It is generated once, at `start`, and rides in
#   $CARRY. Anything else a later stage must agree with goes the same way.
#
#   AN IMAGE IS ITS $RESULT_HASH. The old shape checked a build out, committed
#   it and read the tree hash back with `git rev-parse HEAD:img` — a round trip
#   through the worktree to recover a hash the run already had. A staged test is
#   handed it.
#
# The per-phase timings are gone with the blocking calls: `build` and
# `first-run` are separate container round trips now, so the numbers would
# measure the harness rather than the builder.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The salted sources, rebuilt identically in whichever stage needs them: a pure
# function of $CARRY_OUT/uniq (seeded from $CARRY) and the checked-in fixtures, so every stage that runs
# this gets byte-identical files and therefore the same cache key.
salt_sources() {
  local uniq; uniq=$(cat "$CARRY_OUT/uniq")
  local greeter edited
  greeter=$(<test/greeter.rs)
  edited=$(<test/greeter-edited.rs)
  printf '%s\n' "${greeter//source-built worker/source-built worker $uniq}" > g1.rs
  printf '%s\n' "${edited//different greeting entirely/different greeting entirely $uniq}" > g2.rs
  git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "salted sources"
}

# No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
# the built binary onto it itself, so a caller says only what it is building.
# `--src` is CURRIED because `stage_next` binds its subject to `--in`.
build() { # <source-file> -> the build image
  local builder; builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)
  "$CAOS_CLI" curry --base:hash="$builder" --src:@="$1"
}

case "$STAGE" in

start)
  printf '%s\n' "$(date +%s%N)-$$-$RANDOM" | tr -cd '0-9a-zA-Z\n' > "$CARRY_OUT/uniq"
  salt_sources
  echo "build greeter.rs -> runnable worker -> run" >&2
  stage_next run-first "$(build g1.rs)" g1.rs
  ;;

run-first)
  # $RESULT is the built worker image; run it with nothing bound.
  stage_next check-first "$RESULT_HASH"
  ;;

check-first)
  fetch_result
  grep -q "source-built worker" "$RESULT/greeting" \
    || fail "built worker did not produce the expected output"
  # The original worker's output, kept for the leak check two stages on.
  cp "$RESULT/greeting" "$CARRY_OUT/first-greeting"

  echo "edit source -> a distinct worker" >&2
  salt_sources
  stage_next run-second "$(build g2.rs)" g2.rs
  ;;

run-second)
  stage_next check-second "$RESULT_HASH"
  ;;

check-second)
  fetch_result
  grep -q "different greeting" "$RESULT/greeting" \
    || fail "edited worker did not produce the new output"
  grep -q "different greeting" "$CARRY/first-greeting" \
    && fail "the new output leaked into the original worker's result"
  echo "rust-worker: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
