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
# Target musl: that's the one target the deps bake carries, so tests reuse
# it instead of recompiling the dep graph. musl statics run in the Linux
# worker, so `cargo test` still runs them.
"$CAOS_CLI" run /cas/std/cargo r1 -- --tree:@=ws --cmd=test --mode=all \
  "--target=$(uname -m)-unknown-linux-musl"
if [ "$(cat r1/exit)" != "0" ]; then
  echo "== cargo test FAILED (exit $(cat r1/exit)) — full output ==" >&2
  echo "---- stdout ----" >&2; cat r1/stdout >&2
  echo "---- stderr ----" >&2; cat r1/stderr >&2
  fail "unit tests failed"
fi

# Clippy, same decomposition, same baked deps — so it re-lints only the
# crates an edit touched. It runs HERE rather than as a nix check because
# this is the only runner anyone invokes: `nix flake check` has no CI behind
# it, and clippy had no coverage at all until this. `-D warnings` is applied
# worker-side (worker-cargo), so a lint is a job failure, not a warning.
echo "== cargo clippy of the workspace, per-crate, in a caos worker ==" >&2
"$CAOS_CLI" run /cas/std/cargo r2 -- --tree:@=ws --cmd=clippy --mode=all \
  "--target=$(uname -m)-unknown-linux-musl"
if [ "$(cat r2/exit)" != "0" ]; then
  echo "== cargo clippy FAILED (exit $(cat r2/exit)) — full output ==" >&2
  echo "---- stdout ----" >&2; cat r2/stdout >&2
  echo "---- stderr ----" >&2; cat r2/stderr >&2
  fail "clippy failed"
fi
echo "unit: ALL PASS" >&2
