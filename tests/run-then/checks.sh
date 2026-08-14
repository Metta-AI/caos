#!/bin/bash
# Runs *inside* a bash worker. Asserts the client-side flag validation of the
# two continuation verbs: `map` and `run` are mutually exclusive (each verb
# only accepts its own flags), run-then requires --run, and --catch requires a
# --then to deliver the error to and belongs to run-then alone. Every bad
# invocation must fail *before* recording anything, so /cas/out is still free
# for our own result at the end.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
img=$(printf 'd%.0s' {1..40})   # any well-formed image ref; never run

if caos run-then /cas/args/in --map:hash="$img" --run:hash="$img" 2>/tmp/err; then
  fail "run-then accepted --map"
fi
grep -q 'takes only --run and --then' /tmp/err \
  || fail "wrong error for run-then --map: $(cat /tmp/err)"

if caos run-then /cas/args/in --then:hash="$img" 2>/tmp/err; then
  fail "run-then accepted a missing --run"
fi
grep -q 'needs --run' /tmp/err \
  || fail "wrong error for run-then without --run: $(cat /tmp/err)"

if caos map-then /cas/args/in --run:hash="$img" 2>/tmp/err; then
  fail "map-then accepted --run"
fi
grep -q 'takes only --map and --then' /tmp/err \
  || fail "wrong error for map-then --run: $(cat /tmp/err)"

# --catch has nowhere to deliver the error without a --then, so the flag is
# refused rather than silently degrading to "the failure propagates anyway".
if caos run-then /cas/args/in --run:hash="$img" --catch 2>/tmp/err; then
  fail "run-then accepted --catch without --then"
fi
grep -q 'needs --then' /tmp/err \
  || fail "wrong error for --catch without --then: $(cat /tmp/err)"

# ...and map-then has no catch at all: catching is scoped to the single-valued
# form, where "the step failed" has one unambiguous meaning. (`--catch=1`, not
# the bare flag: map-then declares no markers, so the flag never reaches the
# allowed-name check — it fails earlier, on parse_arg wanting a `=value`.)
if caos map-then /cas/args/in --map:hash="$img" --catch=1 2>/tmp/err; then
  fail "map-then accepted --catch"
fi
grep -q 'takes only --map and --then' /tmp/err \
  || fail "wrong error for map-then --catch: $(cat /tmp/err)"

echo ok > /tmp/ok
caos put /tmp/ok /cas/out
