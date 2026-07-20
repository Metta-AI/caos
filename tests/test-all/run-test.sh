#!/usr/bin/env bash
# Generic per-test runner, INSIDE a testenv worker, as ROOT. Stands up a nested
# caos stack, publishes an inner std, then runs the REAL cli.sh of the test tree
# at /cas/args/test against it — exactly as tests/run.sh does on the host, but
# as a caos job. Keyed by the outer run on (this script, the test tree, the
# binaries, the backend + images), so one such job per test caches independently.
#
# Two backends, chosen per test by /cas/args/backend:
#   process  the chroot-slot backend (phase 3): fast, no containers, but runs
#            only curry-able bin-workers. Inner std = curry(dummy, bin).
#   socket   the socket-delegation backend (phase 4): the inner runnerd delegates
#            worker containers to the OUTER engine via the granted socket, so it
#            runs image-based workers too (e.g. the bash SCRIPT worker). Inner
#            std maps std/bash -> the self-contained bash image built host-side
#            (docker://, passed as /cas/args/bash_image).
#
# Still out: tests whose cli.sh needs host `nix` (they build a worker inline),
# the heavy toolchain-image tests, and the meta nested-stack tests themselves.
set -euo pipefail

fail() {
  echo "RUN-TEST FAIL: $*" >&2
  tail -25 /tmp/server.log 2>/dev/null >&2 || true
  tail -25 /tmp/runnerd.log 2>/dev/null >&2 || true
  exit 1
}

# The runner injects the OUTER run's std/salt; the inner client must not inherit
# them (see design/cargo-workers.md, phase 3 isolation lessons).
unset CAOS_STD CAOS_SALT

caos get -r /cas/args/bins
mkdir -p /pt && cp /cas/args/bins/* /pt/ && chmod +x /pt/*
caos get -r /cas/args/test
caos get /cas/args/backend
BACKEND=$(cat /cas/args/backend)

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

# Client repo the inner std is published from.
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"

if [ "$BACKEND" = process ]; then
  CAOS_SERVER_URL=$INNER CAOS_RUNNER_MODE=process CAOS_RUNNER_ROOT=/tmp/slots \
    CAOS_PROCESS_CAOS=/pt/caos CAOS_PROCESS_WORKER=/pt/worker-runner \
    CAOS_RUNNER_SLOTS=6 /pt/runnerd >/tmp/runnerd.log 2>&1 &
  sleep 1
  grep -q "process slots" /tmp/runnerd.log || fail "runnerd not in process mode"

  # Inner std: each curry-able Rust worker as curry(dummy, bin) — the image is a
  # passthrough placeholder (process mode ignores it; the trampoline runs bin).
  mkdir -p dummy && echo marker > dummy/m
  for n in file-count dirs-only deep-deps rgrep; do cp "/pt/worker-$n" .; done
  git add -A && git commit -qm setup
  base=$(git rev-parse HEAD:dummy)
  entries=""
  for n in file-count dirs-only deep-deps rgrep; do
    h=$(cli curry "$base" -- "--bin:@=worker-$n")
    entries+="040000 tree ${h}"$'\t'"${n}"$'\n'
  done
  stdtree=$(printf '%s' "$entries" | git mktree)

elif [ "$BACKEND" = socket ]; then
  caos get /cas/args/bash_image
  BASH_IMAGE=$(cat /cas/args/bash_image)
  SOCK=${CAOS_ENGINE_SOCKET:?socket backend needs a granted engine socket}
  [ -S "$SOCK" ] || fail "engine socket $SOCK is not a socket"

  # The docker client delegates to the outer engine (DOCKER_HOST); each sibling
  # joins THIS worker's netns so the inner server on 127.0.0.1 is reachable.
  CAOS_SERVER_URL=$INNER CAOS_DOCKER_BIN=docker DOCKER_HOST="unix://$SOCK" \
    CAOS_DOCKER_NETWORK="container:$(cat /etc/hostname)" CAOS_RUNNER_SLOTS=4 \
    /pt/runnerd >/tmp/runnerd.log 2>&1 &
  sleep 1
  grep -q "slots, server" /tmp/runnerd.log || fail "socket runnerd did not start"

  # Inner std: std/bash -> the self-contained bash SCRIPT worker image (real
  # image the outer engine store has; passed through by the inner server).
  git commit -q --allow-empty -m empty
  bashtree=$(cli curry "docker://$BASH_IMAGE" --)
  entries="040000 tree ${bashtree}"$'\t'"bash"$'\n'
  stdtree=$(printf '%s' "$entries" | git mktree)
else
  fail "unknown backend: $BACKEND"
fi

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
