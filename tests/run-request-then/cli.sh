#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# The subject here is a client behaviour that has no existence outside the
# client: the EXACT-REQUEST continuation form of `run-request-then` and the
# `curry`/`run` pipeline that produces the request it continues. There is no
# static artefact to lint and no server endpoint to poke directly — the
# property only appears when a real ArgTree is curried, recorded, and re-run
# against a live inner stack. So $CAOS_CLI is essential, not incidental: it is
# the very thing under test, and every command below drives it deliberately.
#
# Concretely, the client alone owns the invariants this test pins down:
#
#   * IDENTITY. `run` on a curried base must reproduce the SAME 40-char ArgTree
#     hash the request had when built — the client must forward R by its exact
#     hash, not rebuild a near-equivalent request around its image. Only the
#     client computes and forwards that identity; the server just runs what it
#     is handed.
#
#   * then WITHOUT a synthetic --in. When an exact request has a `--then`
#     callback, the client must deliver ONLY the request's result as --result
#     (or its failure as --error), never a fabricated --in. That argument-
#     shaping is the client's job; the callbacks assert on exactly the arg set
#     the client produced.
#
#   * VALIDATION BEFORE A PROMISE. Malformed hashes, `--catch` without
#     `--then`, and unsupported `--run` must be refused by the client up front,
#     before any promise is recorded. This is client-side argument parsing;
#     nothing but the tested client exercises it.
#
#   * FAILURE PROPAGATION AND catch. An exact request's failure must propagate
#     by default (surfacing the worker's exit status) and, with `--catch`, be
#     delivered to the callback as --error. This routing is decided by the
#     client as it continues the request.
#
# A host-side or server-only reconstruction could not observe any of these:
# they are precisely the client's decisions about how a continuation is
# constructed, validated, and delivered. Hence $CAOS_CLI must be the one client
# every command below goes through.
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
