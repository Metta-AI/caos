#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Proves caos-cli ingests only git-tracked files (the nix-flakes rule). The
# harness committed test/ before this script runs, so the file dropped here
# stays untracked; caos-cli must exclude it when it ingests the (now dirty)
# test/ directory. The worker-side assertions live in check.sh, run inside a
# bash worker where the ingested tree is materialized in a real /cas.
set -euo pipefail

printf 'untracked: must not reach the worker\n' >test/tree/untracked.txt

"$CAOS_CLI" run /cas/std/bash -- --script:@=test/check.sh --test:@=test
