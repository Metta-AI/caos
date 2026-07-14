#!/bin/bash
# Runs *inside* a bash worker. Asserts the client-side flag validation of the
# two continuation verbs: `map` and `run` are mutually exclusive (each verb
# only accepts its own flags), and run-then requires --run. Every bad
# invocation must fail *before* recording anything, so /cas/out is still free
# for our own result at the end.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
img=$(printf 'd%.0s' {1..40})   # any well-formed image ref; never run

if caos run-then /cas/args/in -- --map="$img" --run="$img" 2>/tmp/err; then
  fail "run-then accepted --map"
fi
grep -q 'takes only --run and --then' /tmp/err \
  || fail "wrong error for run-then --map: $(cat /tmp/err)"

if caos run-then /cas/args/in -- --then="$img" 2>/tmp/err; then
  fail "run-then accepted a missing --run"
fi
grep -q 'needs --run' /tmp/err \
  || fail "wrong error for run-then without --run: $(cat /tmp/err)"

if caos map-then /cas/args/in -- --run="$img" 2>/tmp/err; then
  fail "map-then accepted --run"
fi
grep -q 'takes only --map and --then' /tmp/err \
  || fail "wrong error for map-then --run: $(cat /tmp/err)"

echo ok > /tmp/ok
caos put /tmp/ok /cas/out
