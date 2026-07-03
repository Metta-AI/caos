#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is
# at /cas/args/test; builtins are at /cas/std/<name>.
#
# The performance testbed: total the leaf files of the committed fixture tree
# (160 nodes, 120 files — every subtree unique, see gen-tree.sh) via
# `fold --post=file-count`, three ways. Salt isolates the cold runs from each
# other (identical leaf `post` requests alias across the two fold
# implementations by design — a feature, but it would contaminate a cold-vs-cold
# comparison); the warm run reuses the wasm salt, so it must be served entirely
# from the result cache. The server's `caos-trace` stderr carries the per-run
# breakdown.
set -euo pipefail
T=/cas/args/test
caos get "$T" # one level: makes $T/tree a placeholder carrying its hash

fail() { echo "FAIL: $*" >&2; exit 1; }
now_ms() { date +%s%3N; }

count() { # <salt> <fold image> <tag> -> echoes the count
  CAOS_SALT="$1" caos run "$2" "/cas/n-$3" -- --post=/cas/std/file-count --in:@="$T/tree"
  caos get -r "/cas/n-$3"
  cat "/cas/n-$3"
}

t0=$(now_ms)
n=$(count "$CAOS_SALT-wasm" /cas/std/fold-wasm wasm)
t1=$(now_ms)
[ "$n" = "120" ] || fail "fold-wasm: expected 120 leaf files, got: $n"

t2=$(now_ms)
n=$(count "$CAOS_SALT-cont" /cas/std/fold cont)
t3=$(now_ms)
[ "$n" = "120" ] || fail "container fold: expected 120, got: $n"

t4=$(now_ms)
n=$(count "$CAOS_SALT-wasm" /cas/std/fold-wasm wasm2)
t5=$(now_ms)
[ "$n" = "120" ] || fail "fold-wasm warm: expected 120, got: $n"

wasm=$((t1 - t0)) cont=$((t3 - t2)) warm=$((t5 - t4))
echo "fold-bench: fold-wasm ${wasm}ms, container ${cont}ms, warm ${warm}ms (160 nodes, 120 files)" >&2
printf 'wasm_ms=%s container_ms=%s warm_ms=%s nodes=160 files=120\n' "$wasm" "$cont" "$warm" >/tmp/result
caos put /tmp/result /cas/out
