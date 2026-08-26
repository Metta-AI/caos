#!/bin/bash
# tests/rgrep — a WORKER test: no client, no repo.
#
# Exercises the rgrep worker directly (no LLM in the loop): a recursive grep
# fold — one job per directory, results a SPARSE TREE (only matching files,
# `linenum:line` content, child results embedded by hash). Covers the sparse
# shape (non-matching files/dirs absent), regex matching, binary skipping, the
# file-scoped blob result, the no-matches empty tree, and the (subtree, pattern)
# cache: an identical second run is served from cache.
#
# FIVE STAGES: a run cannot be waited on, so each assertion is the `then` of the
# run it is about. The cache check compares the two runs' OIDs rather than
# diffing two checkouts — stronger, not weaker, since results are
# content-addressed, and it needs nothing carried but a hash.
#
# The cold/cached timings the old version printed are gone with the blocking
# calls that made them meaningful: each run is a container round trip now, which
# dwarfs what was being measured.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# EVERYTHING A LATER STAGE READS IS FORWARDED: `/cas/args/base` is the reserved
# base entry, not this job's ArgTree.
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --rgrep:@=/cas/args/rgrep --tree:@=/cas/args/tree "$@"; }

rgrep_job() { caos curry --base:@=/cas/args/rgrep --pattern="$1"; }

case "$stage" in

start)
  echo "== whole-tree grep: sparse result, matches only ==" >&2
  caos run-then /cas/args/tree --run:hash="$(rgrep_job 'need.e')" --then:hash="$(next sparse)"
  ;;

sparse)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/a.txt")" = '1:alpha needle one
3:needle again' ] || fail "a.txt matches wrong: $(cat "$R/a.txt")"
  [ "$(cat "$R/sub/c.txt")" = "1:needle in sub" ] || fail "nested match wrong"
  [ "$(cat "$R/dup1/same.txt")" = "1:needle dup" ] || fail "dup1 match wrong"
  diff <(cd "$R/dup1" && find . -type f -exec cat {} +) \
       <(cd "$R/dup2" && find . -type f -exec cat {} +) \
    || fail "identical subtrees produced different results"
  [ ! -e "$R/b.txt" ] || fail "non-matching file present in the sparse tree"
  [ ! -e "$R/bin.dat" ] || fail "binary file was grepped"
  [ ! -e "$R/quiet" ] || fail "matchless subtree present in the sparse tree"
  [ ! -e "$R/sub/none.txt" ] || fail "non-matching nested file present"
  echo "  ok: matches only, binaries skipped, empty subtrees absent" >&2

  echo "== the same grep again: served from cache ==" >&2
  caos run-then /cas/args/tree --run:hash="$(rgrep_job 'need.e')" \
    --then:hash="$(next cached --cold="$(caos hash "$R")")"
  ;;

cached)
  caos get /cas/args/cold
  [ "$(caos hash /cas/args/result)" = "$(cat /cas/args/cold)" ] \
    || fail "cached result differs from the cold one"
  echo "  ok: identical result" >&2

  echo "== file-scoped grep: the match blob itself ==" >&2
  caos get /cas/args/tree
  caos run-then /cas/args/tree/a.txt --run:hash="$(rgrep_job needle)" \
    --then:hash="$(next scoped)"
  ;;

scoped)
  caos get /cas/args/result || fail "reading the result"
  got=$(cat /cas/args/result)
  [ "$got" = '1:alpha needle one
3:needle again' ] || fail "file-scoped matches wrong: $got"
  echo "  ok: blob of linenum:line matches" >&2

  echo "== no matches anywhere: the empty tree ==" >&2
  caos run-then /cas/args/tree --run:hash="$(rgrep_job absent-string)" \
    --then:hash="$(next empty)"
  ;;

empty)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  [ -d "$R" ] || fail "no-match result is not a tree"
  [ -z "$(find "$R" -type f)" ] || fail "no-match result is not empty: $(find "$R" -type f)"
  echo "  ok: empty tree" >&2

  printf 'rgrep: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
