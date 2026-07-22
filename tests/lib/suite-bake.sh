#!/usr/bin/env bash
# The toolchain BAKE, inside the nix-builder worker (phase D2): `nix build`
# the deps-only cargo base image — pinned toolchain + pre-compiled workspace
# deps, no caos/worker (those are stacked on later from the caos-built bins)
# — and push it to the caos registry, returning the digest ref.
#
# The input (--in, run-then) is the BAKE TREE: flake files, manifests,
# lockfiles, and the crates' target entry points as EMPTY files (crane's
# dummy-source pass needs the paths to exist; empty keeps the bake's key
# independent of source CONTENT). So this expensive job — toolchain download
# + dep compilation in an ephemeral nix store, minutes — re-runs only on
# toolchain/lockfile/manifest changes, never on a source edit.
set -euo pipefail

fail() { echo "BAKE FAIL: $*" >&2; exit 1; }

caos get -r /cas/args/in
mkdir /tmp/ws
cp -rL /cas/args/in/. /tmp/ws/

nixf() { nix --extra-experimental-features "nix-command flakes" "$@"; }
nixf build "path:/tmp/ws#caos-worker-cargo-deps-docker" -o /tmp/img \
  || fail "nix build of the deps image"

# skopeo from OUR flake (locked nixpkgs — pure); the image tarball is
# gzipped, which docker-archive can't always read, so unpack first. Push to
# the registry by its on-net name; the returned ref uses the host-visible
# name (same registry, same digest — engines pull it as localhost:5000).
gunzip -c "$(readlink -f /tmp/img)" > /tmp/img.tar
tag="caos-registry:5000/caos:bake-$(cat /etc/hostname)"
nixf shell "path:/tmp/ws#skopeo" -c \
  skopeo --insecure-policy copy --dest-tls-verify=false \
  "docker-archive:/tmp/img.tar" "docker://$tag" >&2 \
  || fail "push to the registry"
digest=$(nixf shell "path:/tmp/ws#skopeo" -c \
  skopeo inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag") \
  || fail "reading the pushed digest"

printf 'localhost:5000/caos@%s' "$digest" > /tmp/ref
caos put /tmp/ref /cas/out
