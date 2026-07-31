#!/bin/bash
# Suite `then`: every test job's result tree arrives under --children (by
# test name): {verdict, output, server.log, runnerd.log, ...} — the complete
# record. Assemble the report — one PASS/FAIL line per test, ending in an
# OK/FAILED banner — and carry the children through verbatim as `results`
# (a symlink put: recorded-hash reuse, no bytes move). The suite job itself
# always SUCCEEDS with a report; the caller decides what a FAILED banner
# means. Failures are values here so one broken test never hides the others.
set -euo pipefail

caos get /cas/args/children
mkdir -p /tmp/rep
passn=0 failn=0
{
  for c in /cas/args/children/*; do
    t=$(basename "$c")
    caos get "/cas/args/children/$t"
    caos get "/cas/args/children/$t/verdict"
    # The test's own wall time, so the report says which test is the long
    # pole. A suite is as slow as its slowest test once the pool is saturated,
    # and that fact was previously only reachable by correlating inner redis
    # logs by hand.
    secs=""
    if [ -e "$c/seconds" ]; then
      caos get "/cas/args/children/$t/seconds"
      secs=" ($(cat "$c/seconds")s)"
    fi
    if grep -q "^RUN-TEST: PASS" "$c/verdict"; then
      echo "PASS tests/$t$secs"; passn=$((passn + 1))
    else
      echo "FAIL tests/$t$secs"; failn=$((failn + 1))
    fi
  done
  echo
  if [ "$failn" -eq 0 ]; then
    echo "SUITE OK: $passn/$((passn + failn)) passed"
  else
    echo "SUITE FAILED: $passn/$((passn + failn)) passed"
  fi
  # A duration is a property of a RUN; a result is a property of its INPUTS.
  # Caching the one inside the other means an unchanged test replays whatever
  # it happened to cost when it last actually ran — which is the number you
  # will keep seeing until something re-keys it, however much faster the tests
  # have since become. Said out loud, because it read as "the tests are still
  # slow" the first time a report replayed times measured while the engine was
  # unpacking a new image across twenty concurrent stacks.
  echo "(times are each test's LAST ACTUAL RUN; an unchanged test is a cache"
  echo " hit and replays the time it recorded then."
  echo " Pass \`--test-salt=\$(date --iso=s)\` to rerun all tests)"
} > /tmp/rep/report
ln -s /cas/args/children /tmp/rep/results
caos put /tmp/rep /cas/out
