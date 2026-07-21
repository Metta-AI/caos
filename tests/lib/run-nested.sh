#!/usr/bin/env bash
# Generic per-test runner, INSIDE a testenv worker, as ROOT. Stands up a nested
# caos stack built from the tree under test, publishes an inner std, then runs
# the REAL cli.sh of the test tree at /cas/args/test against it — exactly as
# tests/run.sh does on the host, but as a caos job. Keyed by the outer run on
# (this script, the test tree, the binaries, the image refs), so one such job
# per test caches independently: an unchanged test is an instant hit and never
# starts a stack at all.
#
# The inner stack is SOCKET-ONLY (design/cargo-workers.md, phase 4): the inner
# runnerd delegates every worker to the OUTER engine via the granted socket, as
# sibling containers sharing this worker's netns — the production pool
# architecture verbatim. Bin-workers run as curry(runner image, bin), image
# workers (bash, the cargo toolchain) run directly; both by content-addressed
# image ID passed in /cas/args, so no inner registry and no image conversion
# (CAOS_IMAGE_RESOLVE=none + the server's docker:// passthrough).
#
# The inner std is the full library the tests use: bash, runner, the Rust
# bin-workers, and the toolchain entries in build-builtins.sh's shape —
# cargo = curry(cargo base, bin), rustc = curry(runner, bin, cargo=<that ref>,
# worker_common=<source tree>). A private redis backs the inner result cache
# (incrementality tests assert real memoization); it starts empty and dies
# with the job, so it shares nothing (the poisoned-shared-redis lesson).
set -euo pipefail

fail() {
  echo "RUN-TEST FAIL: $*" >&2
  tail -25 /tmp/server.log 2>/dev/null >&2 || true
  tail -25 /tmp/runnerd.log 2>/dev/null >&2 || true
  exit 1
}

# The runner injects the OUTER run's std/salt; the inner client must not
# inherit them (see design/cargo-workers.md, phase 3 isolation lessons). Only
# the inner stack is scrubbed — the outer std stays reachable for nested runs
# against the outer server.
unset CAOS_STD CAOS_SALT

caos get -r /cas/args/bins
mkdir -p /pt && cp /cas/args/bins/* /pt/ && chmod +x /pt/*
caos get -r /cas/args/test
caos get -r /cas/args/worker_common
caos get /cas/args/runner_image
caos get /cas/args/bash_image
caos get /cas/args/cargo_image
RUNNER_IMAGE=$(cat /cas/args/runner_image)
BASH_IMAGE=$(cat /cas/args/bash_image)
CARGO_IMAGE=$(cat /cas/args/cargo_image)
SOCK=${CAOS_ENGINE_SOCKET:?the nested stack needs a granted engine socket}
[ -S "$SOCK" ] || fail "engine socket $SOCK is not a socket"

INNER=http://127.0.0.1
cli() { CAOS_SERVER_URL=$INNER /pt/caos-cli "$@"; }

# The inner result cache: a private, empty redis that dies with this job.
redis-server --port 6390 --save '' >/tmp/redis.log 2>&1 &
for _ in $(seq 1 20); do
  redis-cli -p 6390 ping >/dev/null 2>&1 && break
  sleep 0.5
done
redis-cli -p 6390 ping >/dev/null 2>&1 || fail "inner redis never came up"

mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none \
  CAOS_REDIS_ADDR=127.0.0.1:6390 \
  /pt/server >/tmp/server.log 2>&1 &
ok=""
for _ in $(seq 1 30); do
  if git ls-remote "$INNER" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || fail "inner server never came up"

# Inner runnerd: the docker client delegates to the outer engine (DOCKER_HOST);
# each sibling joins THIS worker's netns so the inner server on 127.0.0.1 is
# reachable.
CAOS_SERVER_URL=$INNER CAOS_DOCKER_BIN=docker DOCKER_HOST="unix://$SOCK" \
  CAOS_DOCKER_NETWORK="container:$(cat /etc/hostname)" CAOS_RUNNER_SLOTS=6 \
  /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "slots, server" /tmp/runnerd.log || fail "socket runnerd did not start"

# Client repo the inner std is published from.
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"

# Inner std, as a cheap mktree, in build-builtins.sh's shape.
for b in /pt/worker-*; do cp "$b" .; done
cp -r /cas/args/worker_common ./worker-common
git add -A && git commit -qm setup

entries=""
add() { entries+="040000 tree $1"$'\t'"$2"$'\n'; }
add "$(cli curry "docker://$RUNNER_IMAGE" --)" runner
add "$(cli curry "docker://$BASH_IMAGE" --)" bash
for b in /pt/worker-*; do
  n=${b#/pt/worker-}
  case "$n" in cargo | rustc) continue ;; esac
  add "$(cli curry "docker://$RUNNER_IMAGE" -- "--bin:@=worker-$n")" "$n"
done
cargo_ref=$(cli curry "docker://$CARGO_IMAGE" -- "--bin:@=worker-cargo")
add "$cargo_ref" cargo
add "$(cli curry "docker://$RUNNER_IMAGE" -- "--bin:@=worker-rustc" \
        "--cargo=$cargo_ref" "--worker_common:@=worker-common")" rustc
stdtree=$(printf '%s' "$entries" | git mktree)
git push -q --force caos "$stdtree:refs/caos/std" || fail "publishing inner std"

# Tests that dogfood the workspace (cargo-self) get it as an input tree; stage
# it as the git repo their cli.sh snapshots via $CAOS_PROJECT.
if [ -e /cas/args/workspace ]; then
  caos get -r /cas/args/workspace
  mkdir -p /tmp/ws && cp -r /cas/args/workspace/. /tmp/ws/
  git -C /tmp/ws init -q
  git -C /tmp/ws add -A
  git -C /tmp/ws -c user.email=test@caos -c user.name=caos commit -qm workspace
  export CAOS_PROJECT=/tmp/ws
fi

# Stage the test tree exactly as tests/run.sh does (its contents at ./test),
# then run its real cli.sh against the inner stack. CAOS_BIN_DIR hands the
# tests their helper binaries (they'd otherwise shell out to host nix);
# CAOS_STUB_HOST points workers at in-job stub servers — siblings share this
# worker's netns, so localhost is the stub's address, not the engine host.
export CAOS_BIN_DIR=/pt
export CAOS_STUB_HOST=127.0.0.1
# A real-API test's key arrives as an arg (chat-online; absent = its cli.sh
# self-skips). Siblings share this worker's netns, so they have its egress.
if [ -e /cas/args/api_key ]; then
  caos get /cas/args/api_key
  ANTHROPIC_API_KEY=$(cat /cas/args/api_key)
  export ANTHROPIC_API_KEY
fi
cp -r /cas/args/test ./test
git add -A && git commit -qm testtree
if ! CAOS_CLI=/pt/caos-cli CAOS_SERVER_URL=$INNER bash test/cli.sh >/tmp/test.out 2>&1; then
  cat /tmp/test.out >&2
  fail "cli.sh failed"
fi
cat /tmp/test.out >&2

echo "RUN-TEST: PASS" > /tmp/verdict
caos put /tmp/verdict /cas/out
