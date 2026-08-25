#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# The workspace's UNIT tests (`cargo test`), as just another suite test: the
# per-crate decomposition (mode=all) keys each crate's tests on its pruned
# source closure, so an edit re-tests the touched crates and their
# dependents, not the world. The workspace arrives in this test's wrapper
# (the pruned build tree — what cargo reads), staged as $CAOS_PROJECT.
#
# One of four unit-* tests — test, clippy, doc, fmt — that were a single
# `unit` test running two at a time and then two in sequence. They share
# nothing but the workspace snapshot, so as separate suite tests they land in
# separate outer-pool slots, and a clippy failure no longer hides whether the
# tests pass. All four name the same workspace in caos-tools/test.sh, so they
# re-key together on a Rust edit and none re-keys on anything else.
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

echo "== cargo test of the workspace, per-crate, in caos workers ==" >&2
# `|| ok=0`, not a bare call: under `set -e` a failed run would kill the
# script before the diagnostics below ever print.
ok=1
"$CAOS_CLI" run r1 --base:@=DEEP-DEPS/cargo --tree:@=ws --cmd=test --mode=all \
  "--target=$tgt" >/tmp/r1.log 2>&1 || ok=0
cat /tmp/r1.log >&2
if [ "$ok" = 0 ] || [ ! -e r1/exit ] || [ "$(cat r1/exit)" != "0" ]; then
  echo "== cargo test FAILED (exit $(cat r1/exit 2>/dev/null)) — full output ==" >&2
  # STDERR FIRST, STDOUT LAST. The suite report inlines a failing test's LAST
  # few lines, and for `cargo test` the part worth reading — the failing test
  # names and their panic messages — is on stdout, while stderr is compile
  # chatter. Printing the informative stream last puts it in the excerpt.
  # unit-clippy and unit-doc want the opposite order, and say so.
  echo "---- stderr ----" >&2; cat r1/stderr >&2 || true
  echo "---- stdout ----" >&2; cat r1/stdout >&2 || true
  # Split the outcome: no result at all (the caos run failed, or no exit file)
  # is INFRA — the cargo worker never ran, so this is uncached and retried, not
  # a cached red. A result whose exit is non-zero is the test's own verdict.
  if [ "$ok" = 0 ] || [ ! -e r1/exit ]; then infra "cargo worker did not run"; fi
  fail "unit tests failed"
fi
echo "unit-test: ALL PASS" >&2
