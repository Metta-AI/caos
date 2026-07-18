#!/usr/bin/env bash
# Generic per-test runner, INSIDE a testenv worker, as ROOT. Stands up a nested
# process-mode caos stack, publishes an inner std from the worker binaries at
# /cas/args/bins (curry(dummy, bin), no images/nix), then runs the REAL cli.sh
# of the test tree at /cas/args/test against it — exactly as tests/run.sh does
# on the host, but as a caos job. Keyed by the outer run on (this script, the
# test tree, the binaries), so one such job per test caches independently.
#
# Only tests whose std is these curry-able Rust workers run here; the bash
# script-worker and toolchain-image tests need an image-capable backend
# (podman, phase 4).
set -euo pipefail

fail() {
  echo "RUN-TEST FAIL: $*" >&2
  tail -25 /tmp/server.log 2>/dev/null >&2 || true
  exit 1
}

# The runner injects the OUTER run's std/salt; the inner client must not inherit
# them (see design/cargo-workers.md, phase 3 isolation lessons).
unset CAOS_STD CAOS_SALT

caos get -r /cas/args/bins
mkdir -p /pt && cp /cas/args/bins/* /pt/ && chmod +x /pt/*
caos get -r /cas/args/test

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

CAOS_SERVER_URL=$INNER CAOS_RUNNER_MODE=process CAOS_RUNNER_ROOT=/tmp/slots \
  CAOS_PROCESS_CAOS=/pt/caos CAOS_PROCESS_WORKER=/pt/worker-runner \
  CAOS_RUNNER_SLOTS=6 /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "process slots" /tmp/runnerd.log || fail "runnerd not in process mode"

# Client repo + inner std, published from the worker binaries.
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"
mkdir -p dummy && echo marker > dummy/m
for n in file-count dirs-only deep-deps rgrep; do cp "/pt/worker-$n" .; done
git add -A && git commit -qm setup
dummy=$(git rev-parse HEAD:dummy)
entries=""
for n in file-count dirs-only deep-deps rgrep; do
  h=$(cli curry "$dummy" -- "--bin:@=worker-$n")
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
