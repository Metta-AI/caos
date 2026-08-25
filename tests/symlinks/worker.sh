#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# The assertions here are about what a *worker* sees in a real /cas (symlink
# materialization), so they live in check.sh and run inside a bash worker; this
# script just launches it.
set -euo pipefail

"$CAOS_CLI" run --base:@=DEEP-DEPS/bash --worker1:@=test/check.sh --test:@=test
