#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the file-count worker: a file counts as 1, a tree recurses over its
# children through server-resolved map-then continuations (with itself on both
# sides) and sums the counts — so it totals a tree's leaf files, exercising the
# promise pipeline end to end. The fixture tree/ holds 5 files across nested
# dirs.
#
# The worker is a TEST FIXTURE, not a std entry: this test carries its source
# (./worker.rs) and builds it with std/rustc — memoized, so the compile
# happens once per source edit, not per run.
#
# A STAGED TEST. Every run this test wants goes through `stage_next` rather than
# `$CAOS_CLI run`, so this container is never parked waiting on a job that needs
# a container of its own (dev/run-test/run-test.sh's header). The script runs
# once per stage, dispatching on $STAGE, with the previous run's result at
# $RESULT / $RESULT_HASH and anything it saved at $CARRY.
#
# `$CAOS_CLI curry` stays where it is: currying resolves `.caos-expr` files that
# are themselves curries (std/rustc's is), which is pure client-side evaluation
# — no job, nothing to wait for. Only `run` had to go.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

case "$STAGE" in

start)
  echo "== build the fixture worker from its source ==" >&2
  # No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
  # the built binary onto it itself, so a caller says only what it is building.
  builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)
  # `--src` CURRIED, not passed as the subject: `stage_next` binds its subject
  # to `--in`, and rustc reads `--src`. Currying is how a staged test passes
  # anything the run needs under a name of its own; the subject is only ever
  # the `--in`. (rustc ignores the extra `--in`; it costs a distinct cache key
  # for this build and nothing else.)
  build=$("$CAOS_CLI" curry --base:hash="$builder" --src:@=test/worker.rs)
  stage_next count-tree "$build" test/worker.rs
  ;;

count-tree)
  echo "  ok: fixture worker built" >&2
  # $RESULT is the built image. Keep its oid for the second run, which happens
  # in a container this one will never share memory with.
  printf '%s\n' "$RESULT_HASH" > "$CARRY_OUT/worker"

  echo "== a whole tree totals its leaf files ==" >&2
  stage_next count-file "$RESULT_HASH" test/tree
  ;;

count-file)
  n=$(cat "$RESULT")
  [ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
  echo "  ok: tree -> 5" >&2

  echo "== a single file counts as 1 ==" >&2
  stage_next check "$(cat "$CARRY/worker")" test/tree/a.txt
  ;;

check)
  n=$(cat "$RESULT")
  [ "$n" = "1" ] || fail "expected 1, got: $n"
  echo "  ok: file -> 1" >&2
  echo "file-count: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
