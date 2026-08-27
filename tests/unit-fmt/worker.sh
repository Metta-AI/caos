#!/bin/bash
# tests/unit-fmt — a WORKER test: no client, no repo. It curries the cargo worker
# with the command it wants, runs it over `rust` (everything cargo compiles,
# arriving as a declared dependency), and reads the result.
#
# No --target and no --mode: `cargo fmt` parses rather than compiles, so it
# needs neither the baked dep graph nor a per-crate decomposition.
#
# TWO STAGES: a run cannot be waited on, so the assertion is the `then`.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# --test-salt rides in EVERY stage: bound only at the top, a fresh value would
# re-run the first container and hit the memo for the rest.
next() { caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
  --stage="$1" --test-salt:@=/cas/args/test-salt; }

case "$stage" in

start)
  echo "== cargo fmt of the workspace, in caos workers ==" >&2
  # Target musl: the one target the deps bake carries, so this reuses it
  # instead of recompiling the dep graph.
  tgt="$(uname -m)-unknown-linux-musl"
  img=$(caos curry --base:@=/cas/args/cargo --cmd=fmt) \
    || fail "currying the cargo job"
  caos run-then /cas/args/rust --run:hash="$img" --then:hash="$(next checked)"
  ;;

checked)
  # A cargo job REPORTS rather than fails: a lint or a broken build comes back
  # as a value with a nonzero `exit`, so the run succeeding says nothing about
  # the verdict. (A run that genuinely fails never reaches here — dev/run-test
  # catches it and this test is FAIL with the worker's own stderr.)
  caos get -r /cas/args/result || fail "reading the cargo result"
  if [ ! -e /cas/args/result/exit ] || [ "$(cat /cas/args/result/exit)" != "0" ]; then
    echo "== cargo fmt FAILED (exit $(cat /cas/args/result/exit 2>/dev/null)) ==" >&2
    echo "---- stdout ----" >&2; cat /cas/args/result/stdout >&2 || true
    echo "---- stderr ----" >&2; cat /cas/args/result/stderr >&2 || true
    fail "fmt failed"
  fi
  printf 'unit-fmt: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
