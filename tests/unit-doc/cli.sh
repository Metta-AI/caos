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
# WHY THIS TEST GOES THROUGH THE TESTED CLIENT ($CAOS_CLI), not a plain
# `cargo doc`:
#
# The subject here is NOT merely "do the workspace's docs build clean". If it
# were, a bare `cargo doc` on the host would answer it and this would be a
# lint-shaped test (see tests/std-lint/cli.sh, which never touches $CAOS_CLI).
# It is not: this test PINS DOWN that the tree under test can carry a real
# cargo-doc computation end to end through caos's own machinery — the tested
# client ingesting a tree and a curried `std/cargo` base, submitting a `run` to
# the inner server, the runner materializing the pruned per-crate closures and
# executing rustdoc in a worker, and the result (r3/exit, r3/stdout,
# r3/stderr) coming back addressably. That whole path IS the thing under test;
# `cargo doc` run directly would exercise none of it — not the client's
# ingest/closure walk, not `--base:@`/`--tree:@` resolution, not `--mode=all`
# per-crate decomposition, not the worker's RUSTDOCFLAGS plumbing.
#
# So $CAOS_CLI is ESSENTIAL, not incidental: the test asserts on the value the
# tested client produces from a genuine computation, and swapping in the host's
# `cargo` (or the host's /bin/caos) would test host code where the tested code
# was the entire point (run-test.sh spells out that trap). `cargo doc` also
# happens to be the payload precisely because it makes rustdoc's own diagnostics
# the pass/fail signal riding back through that path — docs building clean is
# the assertion, driving the client to prove it works is the reason it's here.
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
"$CAOS_CLI" run r3 --base:@=DEEP-DEPS/cargo --tree:@=ws --cmd=doc --mode=all \
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
