#!/usr/bin/env bash
# Run a caos integration test that lives in a directory.
#
# All boilerplate lives here: build the CLI, publish the std library, set up a
# throwaway client repo with the test directory committed at ./test. The test
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
# Each test reaches builtins as /cas/std/<name>; <dir>/builtins (optional) lists
# which to publish, one or more per line (default: all of them).
#
# Usage: tests/run.sh <test-dir>
# Requires the dev daemons running (`tilt up` / `caosd`): the caos server :9090,
# redis, registry — and a docker the server can reach.
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <test-dir>" >&2; exit 2; }
DIR=$(cd "$1" && pwd)
PROJECT=$(cd "$(dirname "$0")/.." && pwd)
cd "$PROJECT"
[ -f "$DIR/cli.sh" ] || { echo "no cli.sh in $DIR" >&2; exit 2; }

echo "building caos client..." >&2
nix build .#caos-cli -o result-caos
caosbin=$PROJECT/result-caos/bin/caos-cli
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# A per-run salt threads into every request (hence every cache key), so this run
# is independent of any other without ever clearing Redis.
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

# Publish the builtins this test needs (its `builtins` file, or all of them) so it
# can reach them as /cas/std/<name>.
builtins=()
[ -f "$DIR/builtins" ] && read -ra builtins <<<"$(tr '\n' ' ' <"$DIR/builtins")"
echo "publishing std: ${builtins[*]:-<all>}..." >&2
./build-builtins.sh "${builtins[@]}" >/dev/null

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
