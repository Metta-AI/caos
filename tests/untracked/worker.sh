#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# Proves caos-cli ingests only git-tracked files (the nix-flakes rule). The
# harness committed test/ before this script runs, so the file dropped here
# stays untracked; caos-cli must exclude it when it ingests the (now dirty)
# test/ directory. The worker-side assertions live in check.sh, run inside a
# bash worker where the ingested tree is materialized in a real /cas.
set -euo pipefail

printf 'untracked: must not reach the worker\n' >test/tree/untracked.txt

"$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --test:@=test
