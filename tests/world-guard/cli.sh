#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
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
# The negative cases below therefore use `curl` on purpose — to FORGE a foreign
# world the tested client would never send, you must NOT go through the tested
# client. That is the rejection half of the guard.
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI).
# The acceptance half is the half that cannot be faked. The property this test
# pins down is not "the server accepts the literal string `test`" — it is "the
# server accepts the world THE TESTED CLIENT ACTUALLY STAMPS on its requests,
# and that world is this stack's". curl could only assert about a hardcoded
# string, and a hardcoded string is exactly the failure mode this whole guard
# exists to catch: a host binary (or a stale literal) sails through green while
# the code under test is a different world entirely. So the positive case is
# driven by $CAOS_CLI itself — a real `run` that pushes an arg-tree closure,
# schedules a worker on the inner runnerd, and reads the result back, every hop
# carrying the client's own world stamp. If the tree under test ever changes
# the client's world (the one mismatch that is otherwise SILENT — see above),
# THIS assertion is the one that goes red, because the real client, not a
# stand-in, has to be accepted by the real server. The stack under test is
# `test`, which this script deliberately never hardcodes: the CLI supplying its
# own world, and the suite passing at all, is the proof that a client's own
# world is accepted. The CLI is essential to that proof, not incidental.
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
# The positive case, and the reason this test drives $CAOS_CLI rather than curl:
# only the real client emits the world value the tree under test compiled in.
# A real `run` — push closure, schedule a worker, read the result — is accepted
# end to end iff the tested client's own world matches the stack's. If the guard
# rejected same-world traffic, or the tree bumped the client's world, this fails.
echo hello > file.txt
"$CAOS_CLI" run out --base:@=DEEP-DEPS/bash --worker1:@=test/worker.sh >/dev/null
[ "$(cat out)" = "world-guard ok" ] || fail "same-world job did not run: $(cat out)"
echo "  ok: same-world job ran" >&2

echo "PASS" >&2
