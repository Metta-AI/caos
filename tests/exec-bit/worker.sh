#!/bin/bash
# tests/exec-bit — a WORKER test: no client, no repo, just this script run over
# the fixture in a bash worker.
#
# Proves git's executable bit round-trips through the worker CAS as METADATA,
# not as a placeholder permission: it is recorded on the placeholder (an xattr)
# and only becomes a real +x mode bit once the file is fetched.
#
# `[ -x ]` here is exactly the question "does this file carry the exec bit?":
# with no exec bits set it is false for owner, group, other AND root, and with
# them set it is true — so the assertions hold whether or not the worker is the
# CAS owner.
#
# The fixture is CHECKED IN with its mode rather than chmod'd at runtime. git
# records 100755, and caos ingests git's recorded mode, so this exercises the
# real path rather than one the test arranged for itself.
set -euo pipefail
W=/cas/args/ws
fail() { echo "FAIL: $*" >&2; exit 1; }

# Expand ONE level: the entries appear as unfetched placeholders, not loaded
# content — exactly the state we want to inspect.
caos get "$W"

echo "== an unfetched placeholder is NOT executable, even for an exe file ==" >&2
[ -e "$W/run.sh" ] || fail "run.sh placeholder missing"
[ ! -x "$W/run.sh" ] || fail "placeholder run.sh is +x before it is fetched"
[ ! -x "$W/plain.txt" ] || fail "placeholder plain.txt is +x"
echo "  ok: neither placeholder carries +x" >&2

echo "== an unfetched placeholder is NOT READABLE ==" >&2
# UNLIKE THE `[ -x ]` ASSERTIONS ABOVE, this one depends on the worker not
# being the CAS owner — and that dependency IS the mechanism. `/cas` is
# root-owned and a placeholder is mode 0400 (MODE_PLACEHOLDER_FILE,
# crates/caos/src/lib.rs), so an unprivileged worker cannot read what it has
# not fetched. Legitimate here because this test's image is std/bash, which
# declares no CAOS_WORKER_UID and so runs as the default unprivileged uid.
#
# An image that grants root (std/flake-builder, dev/test-stack, the host stack)
# IS the owner, so a placeholder is readable there and reads as EMPTY — which
# is how dev/run-test came to describe "reads as empty" as the general rule. It
# is not the general rule, and this is the assertion that says so.
#
# A REAL READ, not `[ -r ]`: access(2) answers a question about permissions,
# and the thing worth pinning down is what an actual `cat` does.
if cat "$W/plain.txt" >/dev/null 2>&1; then
  fail "an unfetched placeholder was readable — a worker can read what it has not fetched"
fi
echo "  ok: reading an unfetched placeholder fails" >&2

echo "== fetching restores +x on the executable only ==" >&2
caos get "$W/run.sh"
caos get "$W/plain.txt"
[ -x "$W/run.sh" ] || fail "fetched run.sh is not +x"
[ ! -x "$W/plain.txt" ] || fail "fetched plain.txt gained +x"
# The other half of the assertion above: fetching is what makes content
# readable (MODE_FETCHED_FILE, 0444), so a passing "not readable" check above
# means the placeholder state, not a broken fixture.
cat "$W/plain.txt" >/dev/null 2>&1 || fail "a fetched file is not readable"
echo "  ok: run.sh is +x, plain.txt is not, and fetched content reads" >&2

echo "== put/get round-trips the exec bit ==" >&2
caos put "$W" /cas/exec-roundtrip
caos get -r /cas/exec-roundtrip
[ -x /cas/exec-roundtrip/run.sh ] || fail "run.sh lost +x through put/get"
[ ! -x /cas/exec-roundtrip/plain.txt ] || fail "plain.txt gained +x through put/get"
echo "  ok: +x preserved on run.sh, absent on plain.txt" >&2

printf 'exec-bit: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
