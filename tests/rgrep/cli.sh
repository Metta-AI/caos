#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the rgrep worker directly (no LLM in the loop): a recursive grep
# fold — one job per directory, results a SPARSE TREE (only matching files,
# `linenum:line` content, child results embedded by hash). Covers the sparse
# shape (non-matching files/dirs absent), regex matching, binary skipping,
# the file-scoped blob result, the no-matches empty tree, and the
# (subtree, pattern) cache: an identical second run is served from cache.
#
# A STAGED TEST (dev/run-test/run-test.sh's header): every run goes through
# `stage_next`, so this container never parks on a job. Two consequences:
#
#   * results are TREES in the CAS, not checkouts, so `fetch_result` walks one
#     and the assertions read $RESULT;
#   * the cache check compares the two runs' OIDs rather than diffing two
#     checkouts. Stronger, not weaker: results are content-addressed, so equal
#     hashes ARE equal trees, and it needs nothing carried but a hash.
#
# The cold/cached timings are gone with the blocking calls that made them
# meaningful: each run now costs a container round trip that dwarfs what was
# being measured, so a number here would only mislead.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

rgrep() { # <pattern> -> the image, ready for stage_next
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/rgrep --pattern="$1"
}

case "$STAGE" in

start)
  echo "== whole-tree grep: sparse result, matches only ==" >&2
  stage_next sparse "$(rgrep 'need.e')" test/tree
  ;;

sparse)
  fetch_result
  [ "$(cat "$RESULT/a.txt")" = '1:alpha needle one
3:needle again' ] || fail "a.txt matches wrong: $(cat "$RESULT/a.txt")"
  [ "$(cat "$RESULT/sub/c.txt")" = "1:needle in sub" ] || fail "nested match wrong"
  [ "$(cat "$RESULT/dup1/same.txt")" = "1:needle dup" ] || fail "dup1 match wrong"
  diff <(cd "$RESULT/dup1" && find . -type f -exec cat {} +) \
       <(cd "$RESULT/dup2" && find . -type f -exec cat {} +) \
    || fail "identical subtrees produced different results"
  [ ! -e "$RESULT/b.txt" ] || fail "non-matching file present in the sparse tree"
  [ ! -e "$RESULT/bin.dat" ] || fail "binary file was grepped"
  [ ! -e "$RESULT/quiet" ] || fail "matchless subtree present in the sparse tree"
  [ ! -e "$RESULT/sub/none.txt" ] || fail "non-matching nested file present"
  echo "  ok: matches only, binaries skipped, empty subtrees absent" >&2

  echo "== the same grep again: served from cache ==" >&2
  printf '%s\n' "$RESULT_HASH" > "$CARRY_OUT/cold"
  stage_next cached "$(rgrep 'need.e')" test/tree
  ;;

cached)
  [ "$RESULT_HASH" = "$(cat "$CARRY/cold")" ] \
    || fail "cached result differs from the cold one: $RESULT_HASH vs $(cat "$CARRY/cold")"
  echo "  ok: identical result" >&2

  echo "== file-scoped grep: the match blob itself ==" >&2
  stage_next scoped "$(rgrep needle)" test/tree/a.txt
  ;;

scoped)
  got=$(cat "$RESULT")
  [ "$got" = '1:alpha needle one
3:needle again' ] || fail "file-scoped matches wrong: $got"
  echo "  ok: blob of linenum:line matches" >&2

  echo "== no matches anywhere: the empty tree ==" >&2
  stage_next empty "$(rgrep absent-string)" test/tree
  ;;

empty)
  fetch_result
  [ -d "$RESULT" ] || fail "no-match result is not a tree"
  [ -z "$(find "$RESULT" -type f)" ] \
    || fail "no-match result is not empty: $(find "$RESULT" -type f)"
  echo "  ok: empty tree" >&2

  echo "rgrep: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
