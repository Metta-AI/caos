#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Phase-4 caos-in-caos (design/cargo-workers.md): an inner caos stack runs
# inside a testenv worker, but its runnerd delegates worker containers to the
# OUTER engine through the granted socket (CAOS_RUNNER_SOCKET on the outer
# runnerd) — so the sub-workers run as SIBLINGS, image-based, closing the gap
# the process backend leaves (it can only run bin-workers via the trampoline).
# The sibling image is a self-contained runner we build + tag host-side here;
# the inner server passes docker://<tag> straight through.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== building the inner-stack binaries ==" >&2
nix build "$CAOS_PROJECT#server" -o srv
nix build "$CAOS_PROJECT#runnerd" -o rnd
nix build "$CAOS_PROJECT#caos" -o cs
nix build "$CAOS_PROJECT#worker-runner" -o wr
nix build "$CAOS_PROJECT#worker-rgrep" -o rg

mkdir -p bins
cp -L srv/bin/server rnd/bin/runnerd cs/bin/caos cs/bin/caos-cli \
  wr/bin/worker-runner rg/bin/worker-rgrep bins/
commit "inner-stack binaries"

echo "== building the sibling runner image (self-contained, setuid caos) ==" >&2
# A real image the OUTER engine store can run: static caos (setuid-root, so the
# uid-1000 worker reaches the root-owned /cas xattrs through it) + the runner
# trampoline at /worker. Stable tag so the caos job's inputs don't churn.
RUNNER_IMAGE=localhost/caos-socket-runner:latest
ctx=$(mktemp -d)
cp -L cs/bin/caos "$ctx/caos"
cp -L wr/bin/worker-runner "$ctx/worker"
cat > "$ctx/Containerfile" <<EOF
FROM debian:stable-slim
COPY caos /bin/caos
COPY worker /worker
RUN chmod 4755 /bin/caos && chmod 0755 /worker
EOF
docker build -t "$RUNNER_IMAGE" "$ctx" >/tmp/socket-imgbuild.log 2>&1 \
  || { tail -20 /tmp/socket-imgbuild.log >&2; fail "sibling runner image build failed"; }
rm -rf "$ctx"

echo "== the inner stack as a caos worker job (siblings via the socket) ==" >&2
t0=$(ms)
"$CAOS_CLI" run /cas/std/testenv r1 -- \
  --script:@=test/inner-socket.sh --bins:@=bins --runner_image="$RUNNER_IMAGE"
t1=$(ms)
grep -q "SOCKET-IN-CAOS: ALL PASS" r1 || fail "inner stack did not pass: $(cat r1)"
echo "  ok: socket-delegation caos-in-caos ($((t1 - t0))ms)" >&2

echo "== identical inputs: the test never re-runs ==" >&2
t2=$(ms)
"$CAOS_CLI" run /cas/std/testenv r2 -- \
  --script:@=test/inner-socket.sh --bins:@=bins --runner_image="$RUNNER_IMAGE"
t3=$(ms)
cmp -s r1 r2 || fail "cached verdict differs"
echo "  ok: cache hit ($((t3 - t2))ms vs $((t1 - t0))ms)" >&2
