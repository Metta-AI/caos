#!/usr/bin/env bash
# Runs INSIDE a testenv worker (tests/test-in-caos), as ROOT — the image's
# CAOS_WORKER_UID=0 grant. This is caos-in-caos: start an inner caos stack
# (the server + a process-mode runnerd whose slots are chroots) from the
# binaries that arrived as /cas/args/bins, and drive a recursive rgrep fold
# through it. Our own `caos` still talks to the OUTER server (the runner's
# env); the inner stack's URL is passed per-command, never exported.
set -euo pipefail

fail() { echo "TEST-IN-CAOS FAIL: $*" >&2; exit 1; }

# Materialize the inner-stack binaries (CAS materialization drops the exec
# bit; restore it).
caos get -r /cas/args/bins
mkdir -p /pt
cp /cas/args/bins/* /pt/
chmod +x /pt/*

INNER=http://127.0.0.1

# The runner hands us the OUTER run's std/salt (CAOS_STD names a tree only
# the outer server has); the inner requests must not inherit them, or the
# inner server 404s resolving a std it never saw. Our own `caos` calls
# (get/put against the outer server) don't read them.
unset CAOS_STD CAOS_SALT

echo "== inner server (process backend) =="
# The inner server must NOT share the OUTER stack's redis. This worker's
# container sits on the outer caos-net, where `caos-redis` resolves — and the
# result cache maps request-hash -> object-hash, so a shared cache would hand
# the inner server a hit pointing at an object that lives only in the OUTER
# git repo (the request hash is content-addressed and collides across stacks).
# Point it at a dead local port: cache_get errors -> treated as a miss ->
# every result is computed in, and pinned from, THIS repo. Hermetic.
mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none CAOS_REDIS_ADDR=127.0.0.1:6399 \
  /pt/server >/tmp/server.log 2>&1 &
ok=""
for _ in $(seq 1 30); do
  if git ls-remote "$INNER" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || { cat /tmp/server.log >&2; fail "inner server never came up"; }

echo "== inner process-mode runnerd (chroot slots) =="
CAOS_SERVER_URL=$INNER CAOS_RUNNER_MODE=process CAOS_RUNNER_ROOT=/tmp/slots \
  CAOS_PROCESS_CAOS=/pt/caos CAOS_PROCESS_WORKER=/pt/worker-runner \
  CAOS_RUNNER_SLOTS=4 /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "process slots" /tmp/runnerd.log || { cat /tmp/runnerd.log >&2; fail "runnerd not in process mode"; }

echo "== inner client + workload =="
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"
mkdir -p dummy tree/sub
echo "inner base marker" > dummy/marker
printf 'a needle here\nnothing\n' > tree/a.txt
printf 'no match at all\n' > tree/b.txt
printf 'deep needle too\n' > tree/sub/c.txt
cp /pt/worker-rgrep rgrep-bin
git add -A
git commit -qm 'inner workload'

echo "== rgrep fold through the inner stack =="
curried=$(CAOS_SERVER_URL=$INNER /pt/caos-cli curry "$(git rev-parse 'HEAD:dummy')" \
  -- --bin:@=rgrep-bin --pattern=needle)
if ! CAOS_SERVER_URL=$INNER /pt/caos-cli run "$curried" out -- --in:@=tree 2>/tmp/run.err; then
  cat /tmp/run.err >&2
  tail -20 /tmp/server.log >&2
  fail "inner rgrep run failed"
fi

grep -q '1:a needle here' out/a.txt || fail "flat match missing"
grep -q '1:deep needle too' out/sub/c.txt || fail "recursive match missing"
[ ! -e out/b.txt ] || fail "sparse result carries a matchless file"
echo "  ok: inner stack computed the fold"

echo "INNER-STACK: ALL PASS" > /tmp/verdict
caos put /tmp/verdict /cas/out
