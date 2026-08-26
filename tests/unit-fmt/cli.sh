#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# `cargo fmt --all --check` over the workspace. Flat, not --mode=all:
# formatting is syntactic, so there is no dep graph to decompose over and
# nothing to gain from per-crate keys — rustfmt reads the tree and stops.
#
# WHY THIS GOES THROUGH $CAOS_CLI (and cannot be a plain `cargo fmt` here).
# The subject is not "is the workspace formatted" as a fact a host can check
# by shelling out to rustfmt — that would test rustfmt, not the tree under
# test. The subject is a real caos computation END TO END: the TESTED client
# must ingest the snapshotted tree (ws), resolve `--base=DEEP-DEPS/cargo` — a
# CURRIED cargo image whose base is a blob-bound hash, not a git reference (see
# DEPS) — launch the std/cargo worker into the inner stack, run `--cmd=fmt`
# there, and hand back a result TREE the test then reads by path (r4/exit,
# r4/stderr, r4/stdout). rustfmt's verdict is merely the payload that proves
# the round trip carried an exit code and both streams back faithfully; swap in
# any other `--cmd` and the same client machinery is what is under test. That
# is why the CLI is essential, not incidental: nothing but the tested client
# aimed at the inner server exercises ingest -> curried-base resolution ->
# worker launch -> result-tree return, and a host-side `cargo fmt` would prove
# none of it. The `|| infra` guard and the r4/* reads below are only meaningful
# because a genuine `caos run` is what produced them.
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
"$CAOS_CLI" run r4 --base:@=DEEP-DEPS/cargo --tree:@=ws --cmd=fmt >/tmp/r4.log 2>&1 || ok=0
cat /tmp/r4.log >&2
if [ "$ok" = 0 ] || [ ! -e r4/exit ] || [ "$(cat r4/exit)" != "0" ]; then
  echo "== cargo fmt FAILED (exit $(cat r4/exit 2>/dev/null)) — full output ==" >&2
  # STDOUT LAST: rustfmt prints the offending diff there, and the suite report
  # inlines a failing test's LAST lines.
  echo "---- stderr ----" >&2; cat r4/stderr >&2 || true
  echo "---- stdout ----" >&2; cat r4/stdout >&2 || true
  if [ "$ok" = 0 ] || [ ! -e r4/exit ]; then infra "cargo worker did not run"; fi
  fail "fmt failed"
fi
echo "unit-fmt: ALL PASS" >&2
