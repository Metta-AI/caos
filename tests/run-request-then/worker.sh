#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack (tests/lib stages the repo, then runs this).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== build one complete request R and learn its identity ==" >&2
identify=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/identify.sh)
request=$("$CAOS_CLI" run --base:hash="$identify" --tag=exact-request)
if [ "${#request}" -ne 40 ] || [[ ! "$request" =~ ^[0-9a-f]+$ ]]; then
  fail "identify returned a malformed request hash: $request"
fi
echo "  ok: R=$request" >&2

echo "== an exact tail call runs R unchanged ==" >&2
driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --request="$request")
actual=$("$CAOS_CLI" run --base:hash="$driver")
[ "$actual" = "$request" ] || fail "expected R to report $request, got: $actual"
echo "  ok: R retained its exact ArgTree identity" >&2

echo "== then receives only R's result ==" >&2
callback=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/callback.sh)
driver_then=$("$CAOS_CLI" curry --base:hash="$driver" --then-img="$callback")
actual=$("$CAOS_CLI" run --base:hash="$driver_then")
[ "$actual" = "result=$request" ] || fail "callback result mismatch: $actual"
echo "  ok: callback received --result without a synthetic --in" >&2

echo "== invalid forms are rejected before recording a promise ==" >&2
checks=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/checks.sh --request="$request")
actual=$("$CAOS_CLI" run --base:hash="$checks")
[ "$actual" = "ok" ] || fail "validation checks returned: $actual"
echo "  ok: malformed request, catch-without-then, and --run were refused" >&2

echo "== exact-request failures propagate by default ==" >&2
boom=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/boom.sh)
if "$CAOS_CLI" run --base:hash="$boom" --tag=exact-failure 2>boom.err; then
  fail "expected the failing request to fail"
fi
failed_request=
while IFS= read -r line; do
  case "$line" in
    EXACT_FAIL_REQUEST=*) failed_request=${line#EXACT_FAIL_REQUEST=} ;;
  esac
done < boom.err
if [ "${#failed_request}" -ne 40 ] || [[ ! "$failed_request" =~ ^[0-9a-f]+$ ]]; then
  fail "could not recover the failed request identity from: $(cat boom.err)"
fi

failed_driver=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/driver.sh --request="$failed_request")
if "$CAOS_CLI" run --base:hash="$failed_driver" 2>uncaught.err; then
  fail "exact request failure was swallowed without --catch"
fi
grep -q "exit status: 23" uncaught.err \
  || fail "uncaught failure lost its cause: $(cat uncaught.err)"
echo "  ok: failure propagated" >&2

echo "== catch delivers the exact request's failure as --error ==" >&2
caught_driver=$("$CAOS_CLI" curry --base:hash="$failed_driver" \
  --then-img="$callback" --catch=1)
actual=$("$CAOS_CLI" run --base:hash="$caught_driver")
[ "$actual" = "caught=yes" ] || fail "caught callback returned: $actual"
echo "  ok: callback received --error without --in" >&2

echo "run-request-then: ALL PASS" >&2
