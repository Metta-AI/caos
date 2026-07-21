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
# workers (bash) run directly; both by content-addressed image ID passed in
# /cas/args, so no inner registry and no image conversion
# (CAOS_IMAGE_RESOLVE=none + the server's docker:// passthrough).
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
caos get /cas/args/runner_image
caos get /cas/args/bash_image
RUNNER_IMAGE=$(cat /cas/args/runner_image)
BASH_IMAGE=$(cat /cas/args/bash_image)
SOCK=${CAOS_ENGINE_SOCKET:?the nested stack needs a granted engine socket}
[ -S "$SOCK" ] || fail "engine socket $SOCK is not a socket"

INNER=http://127.0.0.1
cli() { CAOS_SERVER_URL=$INNER /pt/caos-cli "$@"; }

# Inner server: hermetic cache (dead redis port — a shared outer redis would
# hand back hits pointing at objects only the outer git repo has).
mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none CAOS_REDIS_ADDR=127.0.0.1:6399 \
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
  CAOS_DOCKER_NETWORK="container:$(cat /etc/hostname)" CAOS_RUNNER_SLOTS=4 \
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

# Inner std, as a cheap mktree: bash is the real std bash image, bin-workers
# are curry(runner image, bin) — the pool shape — over the binaries under test.
for b in /pt/worker-*; do cp "$b" .; done
git add -A && git commit -qm setup
entries="040000 tree $(cli curry "docker://$BASH_IMAGE" --)"$'\t'"bash"$'\n'
for b in /pt/worker-*; do
  n=${b#/pt/worker-}
  h=$(cli curry "docker://$RUNNER_IMAGE" -- "--bin:@=worker-$n")
  entries+="040000 tree ${h}"$'\t'"${n}"$'\n'
done
stdtree=$(printf '%s' "$entries" | git mktree)
git push -q --force caos "$stdtree:refs/caos/std" || fail "publishing inner std"

# Stage the test tree exactly as tests/run.sh does (its contents at ./test),
# then run its real cli.sh against the inner stack.
cp -r /cas/args/test ./test
git add -A && git commit -qm testtree
if ! CAOS_CLI=/pt/caos-cli CAOS_SERVER_URL=$INNER bash test/cli.sh >/tmp/test.out 2>&1; then
  cat /tmp/test.out >&2
  fail "cli.sh failed"
fi
cat /tmp/test.out >&2

echo "RUN-TEST: PASS" > /tmp/verdict
caos put /tmp/verdict /cas/out
