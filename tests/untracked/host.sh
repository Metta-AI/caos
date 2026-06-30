#!/usr/bin/env bash
# Runs on the host (cwd: the test's committed copy in the client repo) before the
# worker, after the commit — so the file it drops stays untracked. caos-cli must
# exclude it when it ingests the (now dirty) `test/` directory: the nix-flakes
# rule that a build sees only git-tracked files.
set -euo pipefail
printf 'untracked: must not reach the worker\n' >tree/untracked.txt
