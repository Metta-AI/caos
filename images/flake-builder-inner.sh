#!/usr/bin/env bash
# The INNER flake-builder (design/flake-images.md, catch 2): `nix build` a
# user flake's `#caosImage` output into a real image, push it to the caos
# registry, and return the CLEAN image as {ref, config} — no `/caos` in it.
# The caos runner layer is stacked on afterward by the OUTER worker, so this
# job's result is keyed purely on the flake tree and stays valid across every
# caos change.
#
# We return the flake image's OCI config alongside the digest: the OUTER
# worker rebuilds the stacked image's config.json from it, preserving the
# flake's PATH/Env (a rustc worker's toolchain lives at nix-store paths only
# that config names) while forcing the runner entrypoint and adding /bin to
# PATH (where the stacked setuid caos lives).
#
# The input (--in, run-then) is the FLAKE TREE: a directory with flake.nix +
# flake.lock exposing `packages.<system>.caosImage` (a dockerTools image).
# Generalizes caos-tools/lib/bake.sh — the durable memo IS the registry tag
# (`flake-<treehash>`), content-addressed and self-healing: a re-fired job
# finds the previous build in seconds and `nix build` runs only when the flake
# tree genuinely changed (or the registry was pruned).
set -euo pipefail

# skopeo (and nix) resolve $HOME; the worker runs as root in a minimal container
# where a uid-0 lookup can fail ("unknown userid 0"), so set it explicitly to the
# world-writable /tmp the worker owns.
export HOME=/tmp

fail() { echo "FLAKE BUILD FAIL: $*" >&2; exit 1; }

caos get -r /cas/args/in
[ -e /cas/args/in/flake.nix ] || fail "no flake.nix in the flake tree"
[ -e /cas/args/in/flake.lock ] || fail "no flake.lock in the flake tree"
mkdir /tmp/ws
cp -rL /cas/args/in/. /tmp/ws/

H=$(caos hash /cas/args/in) || fail "hashing the flake tree"
tag="caos-registry:5000/caos:flake-$H"
# Single-user nix: the worker runs as root in a container with no `nixbld`
# build-users group (our caos layer overwrites nixos/nix's /etc/group) and no
# privilege to set up a build sandbox — so build as the calling user, unsandboxed.
nixf() {
  nix --extra-experimental-features "nix-command flakes" \
    --option build-users-group "" --option sandbox false "$@"
}
# skopeo is baked into THIS image (not pulled from the user's flake): a general
# flake makes no `#skopeo` promise.
sk() { skopeo --insecure-policy "$@"; }

# Already built for this exact flake tree? (The tag is the content hash.)
if digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag" 2>/dev/null); then
  echo "flake-build: registry hit for $tag" >&2
else
  nixf build -L "path:/tmp/ws#caosImage" -o /tmp/img \
    || fail "nix build of path:/tmp/ws#caosImage (does the flake expose packages.<system>.caosImage?)"
  # The image tarball is gzipped, which docker-archive can't always read, so
  # unpack first. Push by the registry's on-net name; the returned ref uses
  # the host-visible name (same registry, same digest).
  gunzip -c "$(readlink -f /tmp/img)" > /tmp/img.tar
  sk copy --dest-tls-verify=false "docker-archive:/tmp/img.tar" "docker://$tag" >&2 \
    || fail "push to the registry"
  digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag") \
    || fail "reading the pushed digest"
fi

mkdir /tmp/out
# The on-network registry name: this ref becomes the delta's `base`, which the
# SERVER pulls at convert time — and from inside the server container the
# registry is caos-registry:5000, not the host-published localhost:5000.
printf 'caos-registry:5000/caos@%s' "$digest" > /tmp/out/ref

# The stacked image's config.json: the flake image's own OCI config with the
# runner entrypoint forced (runnerd overrides Entrypoint at run time anyway,
# but a direct run should still work) and /bin appended to PATH so the stacked
# setuid caos is found. The clean image and its registry tag are untouched, so
# this config-massaging does not perturb the flake-keyed memo above.
sk inspect --tls-verify=false --config "docker://$tag" > /tmp/cfg.json \
  || fail "reading the built image config"
jq '
  (.config.Env // []) as $env
  | ($env | map(select(startswith("PATH="))) | .[0] // "PATH=/usr/bin:/bin") as $path
  | .config.Entrypoint = ["/bin/caos", "runner"]
  | .config.Env = (($env | map(select(startswith("PATH=") | not))) + [$path + ":/bin"])
' /tmp/cfg.json > /tmp/out/config \
  || fail "rewriting the image config"

caos put /tmp/out /cas/out
