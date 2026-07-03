#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is
# at /cas/args/test; builtins are at /cas/std/<name>.
#
# fold-wasm is the isolate-class fold: same (pre, post, in) contract as
# /cas/std/fold, but a wasm module on the isolate host, fanning children out
# concurrently. Asserts:
#   1. parity — fold-wasm and container fold agree on the wide fixture
#      (30 leaf files; the symlink-to-file counts as a leaf for both);
#   2. depth — the 24-level deep fixture completes (deeper than any bounded
#      worker pool; impossible if fold frames hold container slots);
#   3. memoization — an identical rerun is served from cache.
set -euo pipefail
T=/cas/args/test
caos get "$T"

fail() { echo "FAIL: $*" >&2; exit 1; }
now_ms() { date +%s%3N; }

count() { # <image> <tag> <src> -> echoes the count
  caos run "$1" "/cas/n-$2" -- --post=/cas/std/file-count --in:@="$3"
  caos get -r "/cas/n-$2"
  cat "/cas/n-$2"
}

echo "== parity: fold-wasm vs container fold on wide/ ==" >&2
# 31 = 30 files + the symlink: a leaf blob arrives as a placeholder *file*, so
# file-count counts a symlink leaf like any other blob (matching container fold).
t0=$(now_ms)
n_wasm=$(count /cas/std/fold-wasm wasm-wide "$T/wide")
t1=$(now_ms)
n_cont=$(count /cas/std/fold cont-wide "$T/wide")
t2=$(now_ms)
[ "$n_wasm" = "$n_cont" ] || fail "fold-wasm=$n_wasm != fold=$n_cont"
[ "$n_wasm" = "31" ] || fail "expected 31 leaves, got $n_wasm"
echo "  ok: both count 31 (wasm ${t0}..${t1}, container ${t1}..${t2})" >&2

echo "== depth: 24-level chain completes on the isolate host ==" >&2
n_deep=$(count /cas/std/fold-wasm wasm-deep "$T/deep")
[ "$n_deep" = "24" ] || fail "deep: expected 24, got $n_deep"
echo "  ok: deep -> 24" >&2

echo "== memoization: identical rerun is a cache hit ==" >&2
t3=$(now_ms)
n_again=$(count /cas/std/fold-wasm wasm-wide2 "$T/wide")
t4=$(now_ms)
[ "$n_again" = "31" ] || fail "rerun: expected 31, got $n_again"
warm=$((t4 - t3))
[ "$warm" -lt 2000 ] || fail "rerun took ${warm}ms — not served from cache?"
echo "  ok: rerun 30 in ${warm}ms" >&2

wasm_cold=$((t1 - t0)) cont_cold=$((t2 - t1))
echo "fold-wasm: ALL PASS (wasm ${wasm_cold}ms vs container ${cont_cold}ms on wide/)" >&2
printf 'wasm_ms=%s container_ms=%s warm_ms=%s\n' "$wasm_cold" "$cont_cold" "$warm" >/tmp/result
caos put /tmp/result /cas/out
