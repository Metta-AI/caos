#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
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

# Target musl: that's the one target the deps bake carries, so tests reuse
# it instead of recompiling the dep graph. musl statics run in the Linux
# worker, so `cargo test` still runs them.
tgt="$(uname -m)-unknown-linux-musl"

# BOTH PASSES AT ONCE. They share no state — each is an independent job tree
# that fans out per-crate — and this test is the suite's long pole (59s
# measured, against a 75s suite), so its two halves running strictly in
# sequence set the floor for the whole run. Tempered expectation: each pass
# already saturates the inner 6-slot pool, so what this recovers is each
# pass's TAIL, not half the time.
#
# Clippy runs HERE rather than as a nix check because this is the only runner
# anyone invokes: `nix flake check` has no CI behind it, and clippy had no
# coverage at all until this. `-D warnings` is applied worker-side
# (worker-cargo), so a lint is a job failure, not a warning.
echo "== cargo test + clippy of the workspace, per-crate, in caos workers ==" >&2
test_ok=1
clippy_ok=1
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=ws --cmd=test --mode=all \
  "--target=$tgt" >/tmp/r1.log 2>&1 &
p1=$!
"$CAOS_CLI" run /cas/std/cargo r2 -- --tree:@=ws --cmd=clippy --mode=all \
  "--target=$tgt" >/tmp/r2.log 2>&1 &
p2=$!
# `|| flag=0`, not a bare `wait`: under `set -e` a failed job's non-zero wait
# would kill the script before the diagnostics below ever print.
wait "$p1" || test_ok=0
wait "$p2" || clippy_ok=0
cat /tmp/r1.log /tmp/r2.log >&2

report() { # <label> <result-dir>
  echo "== cargo $1 FAILED (exit $(cat "$2/exit" 2>/dev/null)) — full output ==" >&2
  echo "---- stdout ----" >&2; cat "$2/stdout" >&2 || true
  echo "---- stderr ----" >&2; cat "$2/stderr" >&2 || true
}
if [ "$test_ok" = 0 ] || [ ! -e r1/exit ] || [ "$(cat r1/exit)" != "0" ]; then
  report test r1
  fail "unit tests failed"
fi
if [ "$clippy_ok" = 0 ] || [ ! -e r2/exit ] || [ "$(cat r2/exit)" != "0" ]; then
  report clippy r2
  fail "clippy failed"
fi
echo "unit: ALL PASS" >&2
