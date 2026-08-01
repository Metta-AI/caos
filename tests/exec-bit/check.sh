#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by this test's cli.sh). The ingested
# workspace is at /cas/args/ws in a real /cas.
#
# `[ -x ]` here is exactly the question "does this file carry the exec bit?":
# with no exec bits set it is false for owner, group, other AND root, and with
# them set it is true — so the assertions hold whether or not the worker is the
# CAS owner.
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

echo "== fetching restores +x on the executable only ==" >&2
caos get "$W/run.sh"
caos get "$W/plain.txt"
[ -x "$W/run.sh" ] || fail "fetched run.sh is not +x"
[ ! -x "$W/plain.txt" ] || fail "fetched plain.txt gained +x"
echo "  ok: run.sh is +x, plain.txt is not" >&2

echo "== put/get round-trips the exec bit ==" >&2
caos put "$W" /cas/exec-roundtrip
caos get -r /cas/exec-roundtrip
[ -x /cas/exec-roundtrip/run.sh ] || fail "run.sh lost +x through put/get"
[ ! -x /cas/exec-roundtrip/plain.txt ] || fail "plain.txt gained +x through put/get"
echo "  ok: +x preserved on run.sh, absent on plain.txt" >&2

echo "exec-bit: ALL PASS" >&2
