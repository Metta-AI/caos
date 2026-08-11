#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# `cargo doc` over the workspace, per-crate (mode=all) — the same
# decomposition as unit-test and unit-clippy. `-D warnings` is applied
# worker-side (RUSTDOCFLAGS, scoped to this cmd so doctests are unaffected),
# and --no-deps keeps it to our own docs.
#
# One of four unit-* tests — see tests/unit-test/cli.sh for why they are four.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "workspace snapshot"

tgt="$(uname -m)-unknown-linux-musl"

echo "== cargo doc of the workspace, per-crate, in caos workers ==" >&2
ok=1
"$CAOS_CLI" run DEEP-DEPS/cargo r3 -- --tree:@=ws --cmd=doc --mode=all \
  "--target=$tgt" >/tmp/r3.log 2>&1 || ok=0
cat /tmp/r3.log >&2
if [ "$ok" = 0 ] || [ ! -e r3/exit ] || [ "$(cat r3/exit)" != "0" ]; then
  echo "== cargo doc FAILED (exit $(cat r3/exit 2>/dev/null)) — full output ==" >&2
  # STDERR LAST: rustdoc's warnings-as-errors land there, and the suite report
  # inlines a failing test's LAST lines.
  echo "---- stdout ----" >&2; cat r3/stdout >&2 || true
  echo "---- stderr ----" >&2; cat r3/stderr >&2 || true
  if [ "$ok" = 0 ] || [ ! -e r3/exit ]; then infra "cargo worker did not run"; fi
  fail "doc failed"
fi
echo "unit-doc: ALL PASS" >&2
