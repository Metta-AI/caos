#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# Exercises the rgrep worker directly (no LLM in the loop): a recursive grep
# fold — one job per directory, results a SPARSE TREE (only matching files,
# `linenum:line` content, child results embedded by hash). Covers the sparse
# shape (non-matching files/dirs absent), regex matching, binary skipping,
# the file-scoped blob result, the no-matches empty tree, and the
# (subtree, pattern) cache: an identical second run is served from cache.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; } # epoch milliseconds

echo "== whole-tree grep: sparse result, matches only ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/rgrep out -- --pattern='need.e' --in:@=test/tree
t1=$(ms)
[ "$(cat out/a.txt)" = '1:alpha needle one
3:needle again' ] || fail "a.txt matches wrong: $(cat out/a.txt)"
[ "$(cat out/sub/c.txt)" = "1:needle in sub" ] || fail "nested match wrong"
[ "$(cat out/dup1/same.txt)" = "1:needle dup" ] || fail "dup1 match wrong"
diff <(cd out/dup1 && find . -type f -exec cat {} +) \
     <(cd out/dup2 && find . -type f -exec cat {} +) \
  || fail "identical subtrees produced different results"
[ ! -e out/b.txt ] || fail "non-matching file present in the sparse tree"
[ ! -e out/bin.dat ] || fail "binary file was grepped"
[ ! -e out/quiet ] || fail "matchless subtree present in the sparse tree"
[ ! -e out/sub/none.txt ] || fail "non-matching nested file present"
echo "  ok: matches only, binaries skipped, empty subtrees absent" >&2

echo "== the same grep again: served from cache == XX" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/rgrep out2 -- --pattern='need.e' --in:@=test/tree
t3=$(ms)
diff -r out out2 || fail "cached result differs from the cold one"
echo "  ok: identical result" >&2

echo "== file-scoped grep: the match blob itself ==" >&2
got=$("$CAOS_CLI" run /cas/std/rgrep -- --pattern=needle --in:@=test/tree/a.txt)
[ "$got" = '1:alpha needle one
3:needle again' ] || fail "file-scoped matches wrong: $got"
echo "  ok: blob of linenum:line matches" >&2

echo "== no matches anywhere: the empty tree ==" >&2
"$CAOS_CLI" run /cas/std/rgrep out3 -- --pattern=absent-string --in:@=test/tree
[ -d out3 ] || fail "no-match result did not check out as a directory"
[ -z "$(find out3 -type f)" ] || fail "no-match result is not empty: $(find out3 -type f)"
echo "  ok: empty tree" >&2

# The cold tree run is ~13 jobs through the promise pipeline (5 dirs' greps +
# fan-out/combine steps); the cached rerun collapses to a lookup.
echo "rgrep perf (ms):" >&2
echo "  cold=$((t1 - t0))  cached=$((t3 - t2))" >&2
echo "rgrep: ALL PASS" >&2
