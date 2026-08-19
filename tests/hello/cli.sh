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

echo "== a base path that exists only in an EVALUATED tree resolves ==" >&2
# `--base:@=` descends from the WORKSPACE ROOT through every `.caos-expr` on the
# way, rather than ingesting the named directory and evaluating it alone. The
# difference is invisible unless an ANCESTOR of the path is itself evaluable, so
# this builds that shape: `outer/` produces a tree whose `tool` entry is the
# hello entry, and nothing is at `outer/tool` on disk.
#
# Under the old behaviour this could not work at all — `outer/tool` is not a
# directory, so resolution stopped before evaluating anything. It is the same
# asymmetry `:@@=` had (design/flake-inputs.md, 4a): an entry reachable
# remotely but not locally.
mkdir -p outer/src
cp -r DEEP-DEPS/hello outer/src/hello
cp -r DEEP-DEPS/bash outer/bash
cat > outer/lift.sh <<'LIFT'
#!/bin/bash
set -euo pipefail
caos get /cas/args/src
mkdir -p /tmp/lifted
# By REFERENCE: `caos put` resolves the symlink to the recorded hash, so the
# entry is renamed, not copied.
ln -s /cas/args/src/hello /tmp/lifted/tool
caos put /tmp/lifted /cas/out
LIFT
cat > outer/.caos-expr <<'EXPR'
run --base:@=bash --worker1:@=lift.sh --src:@=src
EXPR
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm hello-nested

[ ! -e outer/tool ] || fail "fixture stale: outer/tool exists on disk, so this proves nothing"
nested=$("$CAOS_CLI" run --base:@=outer/tool --color=blue) || fail "nested base failed: $nested"
printf '%s\n' "$nested" | grep -qx '  color = blue' \
  || fail "the lifted entry did not run as an image: $nested"
echo "  ok: outer/'s expression ran, then tool/'s — neither is on disk as a base" >&2
