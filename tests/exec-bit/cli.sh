#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI, and `caos`
# in the worker):
#
# The subject here is NOT "does a shell preserve a mode bit" — that would be a
# tautology about chmod. The subject is a property of the TESTED CLIENT'S OWN
# ingest/CAS machinery: git's executable bit must round-trip through the worker
# CAS as METADATA, not as a placeholder file permission. Concretely, the client
# must
#   1. INGEST the +x it reads from the git tree and record it on the CAS
#      placeholder as an xattr — NOT as a real +x on the placeholder itself
#      (an unfetched placeholder is deliberately not executable);
#   2. MATERIALIZE that xattr back into a real +x mode bit only when the file is
#      actually fetched (`caos get`); and
#   3. PRESERVE it across a full `caos put` / `caos get` round-trip.
#
# Every one of those steps lives entirely inside the client's own code: the
# split between "placeholder mode" and "recorded mode", the xattr encoding, and
# the fetch-time materialization are things ONLY the tested client does. Nothing
# outside it (git, the filesystem, a lint over the tree) can witness them —
# there is no artifact to inspect statically, because the whole point is the
# behavior of the placeholder representation in a live /cas. So the CLI is
# ESSENTIAL, not incidental: this test exists precisely to pin down how the
# tested client ingests and preserves file modes, and a build that got the
# xattr/materialization wrong would pass any check that did not drive it.
#
# The assertions are about the modes a *worker* sees in a real /cas, so they
# live in check.sh and run inside a bash worker (via `caos`, the same tested
# client, aimed at the inner server); this script just builds the workspace and
# launches it through $CAOS_CLI run.
set -euo pipefail

# An executable file and a plain one. chmod HERE (not a committed fixture mode)
# so the exec bit is unambiguous regardless of how the harness stored the tree.
mkdir -p ws
printf '#!/bin/sh\necho hi\n' > ws/run.sh
chmod +x ws/run.sh
echo plain > ws/plain.txt
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm exec-bit-ws

"$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --ws:@=ws
