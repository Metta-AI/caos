#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI):
# The property under test IS a behaviour of the tested client. `caos-cli run`
# is what ingests a working directory into CAS, and the rule it must obey — the
# nix-flakes rule — is "ingest git-TRACKED paths only, never the untracked
# spill of a dirty tree". That boundary lives entirely inside the client's
# ingest path; nothing else in the stack decides it. So the only honest way to
# pin it down is to hand the tested client a dirty repo and observe what it
# actually shipped to the inner server. A direct `git ls-files` check would
# only re-test git, not the client; reading CAS by hand would re-implement the
# ingest we are trying to verify. The CLI is the subject, not a convenience.
#
# The setup makes the distinction sharp: the harness committed test/ BEFORE
# this script runs, so the file dropped below stays untracked. caos-cli then
# sees test/ as a dirty-but-tracked directory and must exclude the untracked
# file when it ingests it. If the client wrongly ingested untracked spill, the
# excluded file would reach the worker — the exact regression this pins.
#
# The observation happens where the ingested tree is real: the worker-side
# assertions live in check.sh, run inside a bash worker launched BY the tested
# client (--worker1), where the tree caos-cli actually shipped is materialized
# in a real /cas. What the client sent is what the worker sees, so the worker's
# presence/absence checks are a direct read of the client's ingest decision.
set -euo pipefail

printf 'untracked: must not reach the worker\n' >test/tree/untracked.txt

"$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --test:@=test
