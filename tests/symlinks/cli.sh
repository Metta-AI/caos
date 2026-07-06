#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# The assertions here are about what a *worker* sees in a real /cas (symlink
# materialization), so they live in check.sh and run inside a bash worker; this
# script just launches it.
set -euo pipefail

"$CAOS_CLI" run /cas/std/bash -- --script:@=test/check.sh --test:@=test
