#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by this test's cli.sh). The test directory is at
# /cas/args/test, in a real /cas.
#
# Proves caos-cli ingests only git-tracked files (the nix-flakes rule). The
# fixture tree/ holds one committed file, tracked.txt. cli.sh
# dropped an untracked tree/untracked.txt into the client repo *after* the commit,
# so caos-cli sees `test/` as a dirty-but-tracked directory and must exclude the
# untracked file. The worker therefore gets tracked.txt but never untracked.txt.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"   # materialize the ingested tree so it's readable here

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== the tracked file is visible ==" >&2
[ -f "$T/tree/tracked.txt" ] || fail "tracked.txt was dropped"
[ "$(cat "$T/tree/tracked.txt")" = "tracked" ] || fail "tracked.txt has wrong contents"
echo "  ok: tracked.txt is present" >&2

echo "== the untracked file is NOT visible ==" >&2
[ ! -e "$T/tree/untracked.txt" ] || fail "untracked.txt leaked into the worker"
echo "  ok: untracked.txt was excluded" >&2

echo "untracked: ALL PASS" >&2
