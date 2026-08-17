#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE a test stack — the suite's per-test job (tests/lib/run-test.sh).
#
# `std/hello` is what a person runs to check that a caos installation works, so
# what this pins down is the PROPERTY that makes it useful for that: one
# command, answer on stdout, no output path, no checkout, nothing to go looking
# for. If the result ever became a tree, `run` would refuse to stream it and the
# entry would stop being a smoke test — that is the regression this catches.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== hello streams its arguments back, with no output path ==" >&2
out=$("$CAOS_CLI" run --base:@=DEEP-DEPS/hello --greeting=hi --who=world) \
  || fail "the run failed: $out"
[ "$(printf '%s\n' "$out" | head -1)" = "hello: 2 arguments" ] \
  || fail "unexpected header: $out"
printf '%s\n' "$out" | grep -qx '  greeting = hi' || fail "greeting not mirrored: $out"
printf '%s\n' "$out" | grep -qx '  who = world' || fail "who not mirrored: $out"
echo "  ok: both literals came back verbatim" >&2

echo "== self-reference is not mirrored as an argument ==" >&2
# Two kinds ride in every ArgTree and neither is a caller's input: the reserved
# entries (`base` is the worker's own IMAGE — describing it would drag a whole
# image tree into a smoke test), and `workerN`, which for a compiled std entry
# is the worker's own binary, bound by the curry that makes it runnable. The
# first draft of this worker reported `worker1 = blob … (5055368 bytes)` —
# having fetched all 5 MB to say so.
for leaked in base salt secret-hash worker1; do
  printf '%s\n' "$out" | grep -q "  $leaked = " \
    && fail "$leaked leaked into the report as an argument: $out"
done
echo "  ok: reserved entries and workerN stay out of the report" >&2

echo "== a tree argument is summarized, not walked ==" >&2
mkdir -p t && printf 'x\n' > t/a.txt
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm hello-tree
tree_out=$("$CAOS_CLI" run --base:@=DEEP-DEPS/hello --src:@=t) || fail "tree run failed: $tree_out"
want=$(git rev-parse HEAD:t)
printf '%s\n' "$tree_out" | grep -qx "  src = tree $want" \
  || fail "a tree arg should report its kind and hash, got: $tree_out"
echo "  ok: reported as 'tree <oid>' — the hash the cache keyed on" >&2
