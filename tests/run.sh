#!/usr/bin/env bash
# Run a caos integration test that lives in a directory.
#
# All boilerplate lives here: bring the stack up (`caosd up`, which also
# publishes std), build the CLI, and set up a throwaway client repo with the
# test directory committed at ./test. The test
# itself is <dir>/cli.sh: it runs on the HOST, cwd'd into that repo with
# $CAOS_CLI pointing at the caos-cli binary, and drives computations through
# the CLI (whose top-level run blocks — the one place blocking still exists; a
# worker's `map-then` records a continuation instead). A test whose assertions
# are about what a *worker* sees in a real /cas launches a bash worker itself
# (`"$CAOS_CLI" run /cas/std/bash -- --script:@=... --test:@=test`) with the
# worker-side checks in a second script.
#
# No host-side log scraping: a failing assertion exits non-zero, which fails
# the test and surfaces its stderr.
#
# Every test reaches the whole std library as /cas/std/<name>: publishing all of
# it is a cache hit once the stack is warm (the server repo + build-builtins'
# import cache persist), so there's nothing to gain from per-test subsetting.
#
# Usage: tests/run.sh <test-dir>   (or tests/run-all.sh for every test)
# Self-contained: `caosd up` brings the whole stack up (server, the runner,
# redis, registry) and publishes std, then the test runs against it. Needs only
# nix and a docker / `docker compose` on PATH. Leaves the stack warm for the
# next run; stop it with `caosd down`.
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <test-dir>" >&2; exit 2; }
DIR=$(cd "$1" && pwd)
PROJECT=$(cd "$(dirname "$0")/.." && pwd)
cd "$PROJECT"
[ -f "$DIR/cli.sh" ] || { echo "no cli.sh in $DIR" >&2; exit 2; }

echo "building caos client..." >&2
nix build .#caos-cli -o result-caos
caosbin=$PROJECT/result-caos/bin/caos-cli
# The project root, for tests that need to build more flake outputs (e.g. the
# llm-step suite builds its worker binaries and stub server with nix).
export CAOS_PROJECT=$PROJECT
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request (hence every cache key), so this run
# is independent of any other without ever clearing Redis.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# Bring the whole stack up and publish std (idempotent + warm-fast): starts
# caos-server, the runner, redis and the registry, and publishes all of std, so
# the test reaches any builtin as /cas/std/<name>. A repeat `up` (e.g. once per
# test under run-all.sh) is a no-op bring-up + cache-hit republish; the stack is
# left running for the next test. CAOS_DATA (gitignored) persists it warm.
export CAOS_DATA="${CAOS_DATA:-$PROJECT/.caos-data}"
echo "bringing the stack up (caosd up)..." >&2
nix run .#caosd -- up >&2

# A throwaway client repo with the server as its `caos` remote (the host CLI
# pushes the run request from here). A failing assertion exits the worker
# non-zero, which fails the run; a passing test's result is a single file (e.g.
# perf numbers) that we print below.
CLIENT=$PROJECT/.caos-dev/test-client
rm -rf "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
# No detached auto-gc: a host-driven test commits repeatedly, and a background
# gc would race the cleanup rm below (and can outlive the test).
git -C "$CLIENT" config gc.auto 0
git -C "$CLIENT" config maintenance.auto false
trap 'rm -rf "$CLIENT" 2>/dev/null || rm -rf "$CLIENT"' EXIT

# caos-cli only ingests git-tracked paths (like a nix flake), so copy the test
# directory into the client repo and commit it. The committed `test/` is then a
# clean, tracked path the CLI hashes straight from `HEAD`.
cp -R "$DIR" "$CLIENT/test"
git -C "$CLIENT" add -A
git -C "$CLIENT" -c user.email=test@caos -c user.name=caos commit -qm 'test tree'

# cli.sh runs in the client repo, driving computations through caos-cli (whose
# top-level run blocks; it holds no worker slot). Anything it creates after the
# commit above stays untracked — which a test can exploit deliberately (the
# untracked test drops a file here to prove ingestion excludes it).
echo "running $1..." >&2
( cd "$CLIENT" && CAOS_CLI=$caosbin bash test/cli.sh )
echo "PASS: $1" >&2
