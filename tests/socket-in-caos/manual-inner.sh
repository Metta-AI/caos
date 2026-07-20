#!/usr/bin/env bash
# Manual-harness inner stack (phase 4). Like tests/socket-in-caos/inner-socket.sh
# but /pt is pre-staged and there's no outer caos (no caos get/put). Proves:
# inner server + inner runnerd(socket mode) + sibling image workers reporting
# back over the shared netns.
set -euo pipefail
fail(){ echo "P4-INNER FAIL: $*" >&2; exit 1; }

# git comes from the mounted nix store (debian:stable-slim has bash/coreutils
# but no git); its dir was passed in as P4_GIT_PATH.
export PATH="${P4_GIT_PATH:-}:/pt:$PATH"
command -v git >/dev/null || fail "git not on PATH"

INNER=http://127.0.0.1
SOCK=${CAOS_ENGINE_SOCKET:?}
RUNNER_IMAGE=${CAOS_PHASE4_RUNNER_IMAGE:?}
[ -S "$SOCK" ] || fail "socket $SOCK missing"

echo "== inner server ==" >&2
mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none CAOS_REDIS_ADDR=127.0.0.1:6399 \
  /pt/server >/tmp/server.log 2>&1 &
ok=""
for _ in $(seq 1 30); do git ls-remote "$INNER" >/dev/null 2>&1 && { ok=1; break; }; sleep 1; done
[ -n "$ok" ] || { cat /tmp/server.log >&2; fail "inner server never came up"; }

echo "== inner runnerd (socket delegation) ==" >&2
SELF=$(cat /etc/hostname)
CAOS_SERVER_URL=$INNER CAOS_DOCKER_BIN=podman \
  CAOS_DOCKER_ARGS="--remote --url unix://$SOCK" \
  CAOS_DOCKER_NETWORK="container:$SELF" CAOS_RUNNER_SLOTS=2 \
  /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "slots, server" /tmp/runnerd.log || { cat /tmp/runnerd.log >&2; fail "runnerd not up"; }

echo "== workload ==" >&2
mkdir -p /tmp/client && cd /tmp/client
git init -q .; git config user.email t@c; git config user.name c; git config gc.auto 0
git remote add caos "$INNER"
mkdir -p tree/sub
printf 'a needle here\nnothing\n' > tree/a.txt
printf 'no match at all\n' > tree/b.txt
printf 'deep needle too\n' > tree/sub/c.txt
cp /pt/worker-rgrep rgrep-bin
git add -A; git commit -qm work

echo "== rgrep fold via sibling image workers ==" >&2
curried=$(CAOS_SERVER_URL=$INNER /pt/caos-cli curry "docker://$RUNNER_IMAGE" \
  -- --bin:@=rgrep-bin --pattern=needle)
if ! CAOS_SERVER_URL=$INNER /pt/caos-cli run "$curried" out -- --in:@=tree 2>/tmp/run.err; then
  cat /tmp/run.err >&2; echo '---server---' >&2; tail -25 /tmp/server.log >&2
  echo '---runnerd---' >&2; tail -25 /tmp/runnerd.log >&2; fail "run failed"
fi
grep -q '1:a needle here' out/a.txt || fail "flat match missing"
grep -q '1:deep needle too' out/sub/c.txt || fail "recursive match missing"
[ ! -e out/b.txt ] || fail "matchless file present"
echo "P4-INNER: ALL PASS" >&2
