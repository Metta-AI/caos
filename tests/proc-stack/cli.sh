#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# The process-mode backend (design/cargo-workers.md, phase 3), end to end in a
# stock root container with NO inner docker: the edited server (with
# CAOS_IMAGE_RESOLVE=none) plus a process-mode runnerd whose slots are chroots
# carrying the setuid caos and the runner-pool trampoline. The workload — an
# rgrep fold as curry(<dummy tree>, bin=<binary>) — exercises dispatch, the
# uid fence, args materialization, map-then promise resolution and result
# staging, all through plain processes. This is the inner-stack shape the
# tests-as-caos-jobs milestone runs inside a worker; here it runs in a plain
# container so the backend is validated before the nesting.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== building the inner-stack binaries ==" >&2
nix build "$CAOS_PROJECT#server" -o srv
nix build "$CAOS_PROJECT#runnerd" -o rnd
nix build "$CAOS_PROJECT#caos" -o cs
nix build "$CAOS_PROJECT#worker-runner" -o wr
nix build "$CAOS_PROJECT#worker-rgrep" -o rg
gitstore=$(nix build --inputs-from "$CAOS_PROJECT" nixpkgs#gitMinimal --no-link --print-out-paths)

work=$PWD/proc
mkdir -p "$work"
cp -L srv/bin/server rnd/bin/runnerd cs/bin/caos cs/bin/caos-cli \
  wr/bin/worker-runner rg/bin/worker-rgrep "$work/"
cp test/inner.sh "$work/inner.sh"

echo "== inner stack in a stock container (root, no docker inside) ==" >&2
if ! docker run --rm \
  -v /nix/store:/nix/store:ro \
  -v "$work":/pt \
  -e GIT_STORE="$gitstore" \
  debian:stable-slim sh /pt/inner.sh >inner.log 2>&1; then
  cat inner.log >&2
  fail "inner stack failed"
fi
grep -q "PROC-STACK: ALL PASS" inner.log || { cat inner.log >&2; fail "no pass marker"; }
grep -E "==|ok:" inner.log >&2
