#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# The workspace's UNIT tests (`cargo test`), as just another suite test: the
# per-crate decomposition (mode=all) keys each crate's tests on its pruned
# source closure, so an edit re-tests the touched crates and their
# dependents, not the world. The workspace arrives in this test's wrapper
# (the pruned build tree — what cargo reads), staged as $CAOS_PROJECT.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "workspace snapshot"

echo "== cargo test of the workspace, per-crate, in a caos worker ==" >&2
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=ws --cmd=test --mode=all
[ "$(cat r1/exit)" = "0" ] || fail "unit tests failed: $(tail -c 2000 r1/stderr)"
echo "unit: ALL PASS" >&2
