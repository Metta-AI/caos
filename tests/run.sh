#!/usr/bin/env bash
# Run a caos integration test that lives in a directory.
#
# All boilerplate lives here: build the CLI, publish the std library, set up a
# throwaway client repo + CAS. The test itself is <dir>/test.sh, which runs
# *inside* a bash worker (via /cas/std/bash) with the whole test directory
# available at /cas/args/test. So a test does real caos work — running other
# workers, put/get, curry — in a real /cas, and makes its own assertions (the
# bash worker carries coreutils/diff/grep). No host-side log scraping: a failing
# assertion exits test.sh non-zero, which fails the run and surfaces its stderr.
#
# Each test reaches builtins as /cas/std/<name>; <dir>/builtins (optional) lists
# which to publish, one or more per line (default: all of them). A test may also
# include <dir>/host.sh (optional), a hook that runs on the host before the
# worker — see below.
#
# Usage: tests/run.sh <test-dir>
# Requires the dev daemons running (`tilt up` / `caosd`): the caos server :9090,
# redis, registry — and a docker the server can reach.
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <test-dir>" >&2; exit 2; }
DIR=$(cd "$1" && pwd)
PROJECT=$(cd "$(dirname "$0")/.." && pwd)
cd "$PROJECT"
[ -f "$DIR/test.sh" ] || { echo "no test.sh in $DIR" >&2; exit 2; }

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
trap 'rm -rf "$CLIENT"' EXIT

# caos-cli only ingests git-tracked paths (like a nix flake), so copy the test
# directory into the client repo and commit it. The committed `test/` is then a
# clean, tracked path the CLI hashes straight from `HEAD`.
cp -R "$DIR" "$CLIENT/test"
git -C "$CLIENT" add -A
git -C "$CLIENT" -c user.email=test@caos -c user.name=caos commit -qm 'test tree'

# A test may include a `host.sh` hook: it runs on the host, in the committed test
# copy ($CLIENT/test), *after* the commit and *before* the worker. Anything it
# creates therefore stays untracked — e.g. to prove caos-cli excludes untracked
# files when it ingests `test/` (the nix-flakes rule), which the worker-side
# test.sh then asserts.
if [ -f "$CLIENT/test/host.sh" ]; then
  echo "running $1 host.sh hook..." >&2
  ( cd "$CLIENT/test" && bash host.sh )
fi

# Run the test inside a bash worker: test.sh is the script; the whole directory
# rides along as `test` (materialized at /cas/args/test). No output path is
# passed, so a single-file result is written to stdout (a worker's stderr only
# reaches us on failure, so a passing test reports via its /cas/out file).
echo "running $1 inside a bash worker..." >&2
result=$( cd "$CLIENT" && "$caosbin" run /cas/std/bash -- \
    --script:@="test/test.sh" --test:@="test" )
echo "PASS: $1" >&2
# A test that reports a result (e.g. perf numbers) prints it; one that doesn't
# still passed — don't let the empty-result check poison the exit code.
if [ -n "$result" ]; then printf '%s\n' "$result"; fi
