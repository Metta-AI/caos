#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the WORLD guard (crates/caos-world, design/test-stack-image.md):
# the server rejects a caos client built for the other world. This is the one
# mismatch that is otherwise SILENT — a host client driving the test stack
# passes every test until the tree under test changes the client, and then the
# suite is exercising host code in the one place the tested code was the whole
# point. So the guard gets a test of its own; without one it is an assumption.
#
# `host` is not an arbitrary string here: it is exactly what every host-built
# binary stamps on its requests, so sending it reproduces the real crossing.
# The stack under test is `test`, which this script deliberately does not
# hardcode — the suite passing at all is the proof that a client's own world
# is accepted.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# A syntactically valid object id that does not exist: the request reaches the
# guard, and without the guard it would 404.
MISSING=0000000000000000000000000000000000000000
URL="$CAOS_SERVER_URL/object/$MISSING"

echo "== a host-built client is rejected, not served ==" >&2
code=$(curl -s -o /tmp/host.out -w '%{http_code}' -H 'X-Caos-World: host' "$URL")
[ "$code" = "400" ] || fail "host-world request got $code, want 400: $(cat /tmp/host.out)"
grep -qi "world mismatch" /tmp/host.out \
  || fail "rejection does not name the mismatch: $(cat /tmp/host.out)"
grep -q "host" /tmp/host.out || fail "rejection does not name the client's world"
echo "  ok: 400, $(head -c 120 /tmp/host.out)" >&2

echo "== a request with no world header is left alone ==" >&2
# git smart-HTTP and health probes carry no header and must keep working; the
# guard must not turn them into 400s. A missing object still 404s.
code=$(curl -s -o /tmp/bare.out -w '%{http_code}' "$URL")
[ "$code" = "404" ] || fail "unheadered request got $code, want 404: $(cat /tmp/bare.out)"
echo "  ok: 404 (unguarded, as before)" >&2

echo "== removed ref APIs are not served ==" >&2
for endpoint in read append transaction; do
  code=$(curl -sS -o "/tmp/ref-$endpoint.out" -w '%{http_code}' \
    -X POST \
    "$CAOS_SERVER_URL/ref/$endpoint")
  [ "$code" = "404" ] \
    || fail "/ref/$endpoint got $code, want 404: $(cat "/tmp/ref-$endpoint.out")"
done
echo "  ok: all removed ref APIs return 404" >&2

echo "== the tested client still works against its own stack ==" >&2
# The positive case, through the real client rather than curl: if the guard
# rejected same-world traffic, this would fail.
echo hello > file.txt
"$CAOS_CLI" run out --base:@=DEEP-DEPS/bash --worker1:@=test/worker.sh >/dev/null
[ "$(cat out)" = "world-guard ok" ] || fail "same-world job did not run: $(cat out)"
echo "  ok: same-world job ran" >&2

echo "PASS" >&2
