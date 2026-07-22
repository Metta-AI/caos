#!/bin/bash
# Image-build worker — the suite's image map child, INSIDE a testenv worker
# (phase D1, design/cargo-workers.md). Assembles a worker image from a stock
# base + a files tree (the caos-built binaries and worker entrypoint),
# building THROUGH the granted engine socket (no nesting: the outer engine
# does the build), pushes it to the caos REGISTRY — the content-addressed
# store image bytes belong in; an engine store is a cache that can be pruned
# — and returns the digest ref, which downstream jobs pass through the inner
# server as docker://<ref> and any engine can pull.
#
# The spec (--in): base (a docker ref blob), files/usr/... + files/worker.
set -euo pipefail

fail() { echo "IMG-BUILD FAIL: $*" >&2; exit 1; }
SOCK=${CAOS_ENGINE_SOCKET:?image builds need the granted engine socket}
[ -S "$SOCK" ] || fail "engine socket $SOCK is not a socket"
export DOCKER_HOST="unix://$SOCK"

caos get /cas/args/in
caos get /cas/args/in/base
caos get -r /cas/args/in/files
BASE=$(cat /cas/args/in/base)

ctx=/tmp/img-ctx
mkdir -p "$ctx"
cp -rL /cas/args/in/files "$ctx/files"
cat > "$ctx/Dockerfile" <<EOF
FROM $BASE
COPY files/usr /usr
COPY files/worker /worker
RUN chmod 4755 /usr/bin/caos && chmod 0755 /worker
ENTRYPOINT ["/bin/caos","runner"]
EOF

# The tag is scratch (unique per run — this job's container name); the
# durable identity is the pushed DIGEST, which is content-addressed and
# identical whenever the image is.
tag="localhost:5000/caos:build-$(cat /etc/hostname)"
docker build -t "$tag" "$ctx" >&2 || fail "docker build"
docker push "$tag" >/tmp/push.log 2>&1 || { cat /tmp/push.log >&2; fail "push"; }
cat /tmp/push.log >&2
digest=$(grep -o 'sha256:[0-9a-f]*' /tmp/push.log | tail -1)
[ -n "$digest" ] || fail "no digest in push output"

printf 'localhost:5000/caos@%s' "$digest" > /tmp/ref
caos put /tmp/ref /cas/out
