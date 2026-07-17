#!/bin/sh
# Runs as ROOT inside a stock debian:stable-slim container (tests/proc-stack):
# the process-mode backend end to end, no docker anywhere inside. The caos
# binaries arrive bind-mounted at /pt, git from the host's nix store
# ($GIT_STORE). Starts the inner server (no redis, no registry — the cache is
# best-effort and image conversion is off), a process-mode runnerd whose slots
# are chroots, and drives an rgrep fold through it: a flat match, recursion
# via map-then promises, and sparseness.
set -eu

fail() { echo "PROC-STACK FAIL: $*" >&2; exit 1; }

export PATH="$GIT_STORE/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export CAOS_GIT_DIR=/tmp/git
export CAOS_IMAGE_RESOLVE=none

echo "== inner server (process backend) =="
/pt/server >/tmp/server.log 2>&1 &
export CAOS_SERVER_URL=http://127.0.0.1
ok=""
for _ in $(seq 1 30); do
  if git ls-remote "$CAOS_SERVER_URL" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || { cat /tmp/server.log; fail "server never came up"; }

echo "== process-mode runnerd (chroot slots) =="
export CAOS_RUNNER_MODE=process
export CAOS_RUNNER_ROOT=/tmp/slots
export CAOS_PROCESS_CAOS=/pt/caos
export CAOS_PROCESS_WORKER=/pt/worker-runner
export CAOS_RUNNER_SLOTS=4
/pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "process slots" /tmp/runnerd.log || { cat /tmp/runnerd.log; fail "runnerd not in process mode"; }

echo "== client repo + workload =="
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"
mkdir -p dummy tree/sub
echo "process-backend base marker" > dummy/marker
printf 'a needle here\nnothing\n' > tree/a.txt
printf 'no match at all\n' > tree/b.txt
printf 'deep needle too\n' > tree/sub/c.txt
cp /pt/worker-rgrep rgrep-bin
git add -A
git commit -qm 'proc-stack workload'

echo "== rgrep as curry(dummy, bin) through the process backend =="
curried=$(/pt/caos-cli curry "$(git rev-parse 'HEAD:dummy')" -- --bin:@=rgrep-bin --pattern=needle)
/pt/caos-cli run "$curried" out -- --in:@=tree

grep -q '1:a needle here' out/a.txt || fail "flat match missing: $(cat out/a.txt 2>/dev/null)"
grep -q '1:deep needle too' out/sub/c.txt \
  || fail "recursive match missing (map-then promise through the process backend)"
[ ! -e out/b.txt ] || fail "sparse result carries a matchless file"
echo "  ok: flat + recursive matches, sparse tree"

echo "PROC-STACK: ALL PASS"
