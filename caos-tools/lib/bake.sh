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
# toolchain/lockfile/manifest changes... at the CAOS level. The job's request
# additionally keys on the nix-builder IMAGE (which embeds the freshly built
# caos), so a source edit re-fires this job even though the bake tree is
# unchanged. Hence the registry probe: the pushed tag IS the bake tree's
# content hash, so a re-fired job finds the previous bake in seconds and the
# nix build runs only when the bake tree genuinely changed (or the registry
# was pruned — self-healing).
set -euo pipefail

fail() { echo "BAKE FAIL: $*" >&2; exit 1; }

caos get -r /cas/args/in
mkdir /tmp/ws
cp -rL /cas/args/in/. /tmp/ws/

H=$(caos hash /cas/args/in) || fail "hashing the bake tree"
tag="caos-registry:5000/caos:bake-$H"
nixf() { nix --extra-experimental-features "nix-command flakes" "$@"; }
sk() { nixf shell "path:/tmp/ws#skopeo" -c skopeo --insecure-policy "$@"; }

# Already baked for this exact bake tree? (The tag is the content hash.)
if digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag" 2>/dev/null); then
  echo "bake: registry hit for $tag" >&2
else
  nixf build -L "path:/tmp/ws#caos-worker-cargo-deps-docker" -o /tmp/img \
    || fail "nix build of the deps image"
  # The image tarball is gzipped, which docker-archive can't always read, so
  # unpack first. Push by the registry's on-net name; the returned ref uses
  # the host-visible name (same registry, same digest — engines pull it as
  # localhost:5000).
  gunzip -c "$(readlink -f /tmp/img)" > /tmp/img.tar
  sk copy --dest-tls-verify=false "docker-archive:/tmp/img.tar" "docker://$tag" >&2 \
    || fail "push to the registry"
  digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag") \
    || fail "reading the pushed digest"
fi

printf 'localhost:5000/caos@%s' "$digest" > /tmp/ref
caos put /tmp/ref /cas/out
