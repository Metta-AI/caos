#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
#
# Proves caos-cli ingests only git-tracked files (the nix-flakes rule). The
# harness committed test/ before this script runs, so the file dropped here
# stays untracked; caos-cli must exclude it when it ingests the (now dirty)
# test/ directory. The worker-side assertions live in check.sh, run inside a
# bash worker where the ingested tree is materialized in a real /cas.
set -euo pipefail

printf 'untracked: must not reach the worker\n' >test/tree/untracked.txt

"$CAOS_CLI" run /cas/std/bash -- --script:@=test/check.sh --test:@=test
