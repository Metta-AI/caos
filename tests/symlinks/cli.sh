#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# The assertions here are about what a *worker* sees in a real /cas (symlink
# materialization), so they live in check.sh and run inside a bash worker; this
# script just launches it.
set -euo pipefail

"$CAOS_CLI" run /cas/std/bash -- --script:@=test/check.sh --test:@=test
