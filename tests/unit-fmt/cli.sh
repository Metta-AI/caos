#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# `cargo fmt --all --check` over the workspace. Flat, not --mode=all:
# formatting is syntactic, so there is no dep graph to decompose over and
# nothing to gain from per-crate keys — rustfmt reads the tree and stops.
#
# One of four unit-* tests — see tests/unit-test/cli.sh for why they are four.
# A STAGED TEST (dev/run-test/run-test.sh's header): the cargo job is a
# `stage_next` tail call, so this container exits instead of parking on a
# compile that needs a container of its own.
#
# `--may-fail` KEEPS THE INFRA/VERDICT SPLIT. Without it the harness turns any
# failed run into this test's FAIL, which would erase the distinction the old
# shape drew with `|| ok=0`: a cargo worker that never ran is INFRA (uncached,
# retried), while a worker that ran and reported a nonzero `exit` is the
# verdict. Declaring the failure means the error arrives as $ERROR for this
# script to classify, exactly as before.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

case "$STAGE" in

start)
  mkdir ws
  git -C "$CAOS_PROJECT" archive HEAD | tar -x -C ws
  git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "workspace snapshot"
  # No --target and no --mode: `cargo fmt` parses rather than compiles, so it
  # needs neither the baked dep graph nor a per-crate decomposition. The run
  # this replaces passed neither either.
  echo "== cargo fmt of the workspace, in caos workers ==" >&2
  img=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/cargo --cmd=fmt)
  stage_next checked "$img" ws --may-fail
  ;;

checked)
  # The run itself failed: the cargo worker never produced a value, so nothing
  # was linted and this must not cache as a red.
  if [ -n "$ERROR" ]; then
    cat "$ERROR" >&2
    infra "cargo worker did not run"
  fi
  fetch_result
  if [ ! -e "$RESULT/exit" ] || [ "$(cat "$RESULT/exit")" != "0" ]; then
    echo "== cargo fmt FAILED (exit $(cat "$RESULT/exit" 2>/dev/null)) — full output ==" >&2
    echo "---- stdout ----" >&2; cat "$RESULT/stdout" >&2 || true
    echo "---- stderr ----" >&2; cat "$RESULT/stderr" >&2 || true
    fail "fmt failed"
  fi
  echo "unit-fmt: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
