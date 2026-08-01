#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# `cargo fmt --all --check` over the workspace. Flat, not --mode=all:
# formatting is syntactic, so there is no dep graph to decompose over and
# nothing to gain from per-crate keys — rustfmt reads the tree and stops.
#
# One of four unit-* tests — see tests/unit-test/cli.sh for why they are four.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

mkdir ws
git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
commit "workspace snapshot"

echo "== cargo fmt --check of the workspace, in a caos worker ==" >&2
ok=1
"$CAOS_CLI" run /cas/std/cargo r4 -- --tree:@=ws --cmd=fmt >/tmp/r4.log 2>&1 || ok=0
cat /tmp/r4.log >&2
if [ "$ok" = 0 ] || [ ! -e r4/exit ] || [ "$(cat r4/exit)" != "0" ]; then
  echo "== cargo fmt FAILED (exit $(cat r4/exit 2>/dev/null)) — full output ==" >&2
  # STDOUT LAST: rustfmt prints the offending diff there, and the suite report
  # inlines a failing test's LAST lines.
  echo "---- stderr ----" >&2; cat r4/stderr >&2 || true
  echo "---- stdout ----" >&2; cat r4/stdout >&2 || true
  fail "fmt failed"
fi
echo "unit-fmt: ALL PASS" >&2
