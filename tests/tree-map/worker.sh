#!/bin/bash
# tests/tree-map — a WORKER test: no client, no repo.
#
# Exercises tree-map directly, with ./level as the per-directory `run` worker (a
# file containing the `tag` arg leaves a result file `<tag>:<name>`). The fixture
# covers every position tree-map visits: a directory with files only (sub/deep),
# with subdirectories only (mid), with both (the root, sub), and with nothing
# that matches (quiet), plus two identical subtrees (dup1, dup2).
#
# Covers: the sparse result shape (matching files and the directories on the way
# to them, nothing else, no empty directory); identical subtrees giving
# identical results; an identical second run served from cache; an arg the
# caller bound (`tag`) forwarded to `run`; an arg `run` already binds winning
# over the caller's; a subdirectory and a file as `--in`; and the empty tree
# when nothing matches.
#
# STAGES: a run cannot be waited on, so each assertion is the `then` of the run
# it is about, and a hash to compare against later rides along as an arg.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# EVERYTHING A LATER STAGE READS IS FORWARDED: `/cas/args/base` is the reserved
# base entry, not this job's ArgTree.
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --treemap:@=/cas/args/treemap --level:@=/cas/args/level --tree:@=/cas/args/tree "$@"; }

# tree-map over ./level, the way rgrep curries it over its own worker.
map_job() { caos curry --base:@=/cas/args/treemap --run:@=/cas/args/level --tag="$1"; }

# The result files of a tree, one path per line.
files_of() { (cd "$1" && find . -type f | sort); }

case "$stage" in

start)
  echo "== whole-tree map: sparse result, matches only ==" >&2
  caos run-then /cas/args/tree --run:hash="$(map_job needle)" --then:hash="$(next sparse)"
  ;;

sparse)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  want='./a.txt
./dup1/same.txt
./dup2/same.txt
./mid/leaf/e.txt
./sub/c.txt
./sub/deep/d.txt'
  [ "$(files_of "$R")" = "$want" ] || fail "result files wrong: $(files_of "$R")"
  [ "$(cat "$R/a.txt")" = "needle:a.txt" ] || fail "a.txt result wrong: $(cat "$R/a.txt")"
  [ "$(cat "$R/sub/deep/d.txt")" = "needle:d.txt" ] || fail "deep result wrong"
  [ "$(cat "$R/mid/leaf/e.txt")" = "needle:e.txt" ] || fail "result under a files-less directory wrong"
  [ -z "$(find "$R" -type d -empty)" ] || fail "empty directory in the sparse tree: $(find "$R" -type d -empty)"
  [ ! -e "$R/quiet" ] || fail "matchless subtree present in the sparse tree"
  [ ! -e "$R/b.txt" ] || fail "non-matching file present"
  [ ! -e "$R/sub/none.txt" ] || fail "non-matching nested file present"
  [ "$(caos hash "$R/dup1")" = "$(caos hash "$R/dup2")" ] \
    || fail "identical subtrees produced different results"
  echo "  ok: matches only, empty subtrees absent, identical subtrees agree" >&2

  echo "== the same map again: served from cache ==" >&2
  caos run-then /cas/args/tree --run:hash="$(map_job needle)" \
    --then:hash="$(next cached --cold="$(caos hash "$R")")"
  ;;

cached)
  caos get /cas/args/cold
  [ "$(caos hash /cas/args/result)" = "$(cat /cas/args/cold)" ] \
    || fail "cached result differs from the cold one"
  echo "  ok: identical result" >&2

  echo "== another caller arg is forwarded to run ==" >&2
  caos run-then /cas/args/tree --run:hash="$(map_job alpha)" --then:hash="$(next forwarded --cold="$(cat /cas/args/cold)")"
  ;;

forwarded)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  [ "$(files_of "$R")" = './a.txt' ] || fail "alpha result files wrong: $(files_of "$R")"
  [ "$(cat "$R/a.txt")" = "alpha:a.txt" ] || fail "forwarded tag not used: $(cat "$R/a.txt")"
  echo "  ok: forwarded arg reaches run" >&2

  echo "== an arg run already binds wins over the caller's ==" >&2
  caos get /cas/args/cold
  bound=$(caos curry --base:@=/cas/args/level --tag=needle)
  caos run-then /cas/args/tree \
    --run:hash="$(caos curry --base:@=/cas/args/treemap --run:hash="$bound" --tag=alpha)" \
    --then:hash="$(next bound --cold="$(cat /cas/args/cold)")"
  ;;

bound)
  caos get /cas/args/cold
  [ "$(caos hash /cas/args/result)" = "$(cat /cas/args/cold)" ] \
    || fail "the caller's tag overrode the one run binds"
  echo "  ok: run's own binding wins" >&2

  echo "== a subdirectory as --in ==" >&2
  caos get /cas/args/tree
  caos run-then /cas/args/tree/sub --run:hash="$(map_job needle)" --then:hash="$(next subdir)"
  ;;

subdir)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  [ "$(files_of "$R")" = './c.txt
./deep/d.txt' ] || fail "subdirectory result files wrong: $(files_of "$R")"
  echo "  ok: subdirectory scoped" >&2

  echo "== a file as --in: run's result is the result ==" >&2
  caos get /cas/args/tree
  caos run-then /cas/args/tree/a.txt --run:hash="$(map_job needle)" --then:hash="$(next file)"
  ;;

file)
  caos get /cas/args/result || fail "reading the result"
  got=$(cat /cas/args/result)
  [ "$got" = 'file:needle' ] || fail "file result wrong: $got"
  echo "  ok: file handed to run as it is" >&2

  echo "== no matches anywhere: the empty tree ==" >&2
  caos run-then /cas/args/tree --run:hash="$(map_job absent-string)" --then:hash="$(next empty)"
  ;;

empty)
  R=/cas/args/result
  caos get -r "$R" || fail "reading the result"
  [ -d "$R" ] || fail "no-match result is not a tree"
  [ -z "$(find "$R" -type f)" ] || fail "no-match result is not empty: $(find "$R" -type f)"
  [ -z "$(find "$R" -mindepth 1)" ] || fail "no-match result has directories: $(find "$R" -mindepth 1)"
  echo "  ok: empty tree" >&2

  printf 'tree-map: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
