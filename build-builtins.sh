#!/usr/bin/env bash
# Populate the caos `std` library — the workers clients reach as
# `/cas/std/<name>` — and publish it to the server as `refs/caos/std`.
#
# Entries come in three forms (see the is_*_entry predicates below):
#   streamed  (flake-builder)               nix-built, composed onto its stock
#                                           base and pushed to the registry; the
#                                           entry is a curry over the digest ref
#   flake     (runner, cargo-base,          generated flake trees (each
#              bash-base, testenv-base)     std/<name>/stage-tree.sh); the
#                                           flake-builder images them on first use
#   curry     (bash, testenv + bin_names)   curry(<base>, bin=…) — script workers
#                                           bind images/bash-worker.sh, binary
#                                           workers their static musl binary
# The final `{name: entry}` tree is pushed to the server under `refs/caos/std`
# (uploading every referenced object, negotiated). Clients then `git fetch
# caos refs/caos/std` and resolve it locally to reach the library.
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# Requires the dev server running and git + jq + docker on PATH.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

names=("$@")
[ ${#names[@]} -eq 0 ] && names=(runner cargo-base bash-base testenv-base bash testenv flake-builder)

# std entries that are FLAKE TREES, not worker images (design/flake-images.md):
# published as the bake tree itself (std/<name>/stage-tree.sh); the server's
# flake-builder turns it into an image on first use. Everything below that
# builds/imports nix images skips these.
is_flake_entry() { case "$1" in runner | cargo-base | bash-base | testenv-base) return 0 ;; *) return 1 ;; esac; }
# std entries that are SCRIPT WORKERS over a flake base: curry(<name>-base,
# bin=images/bash-worker.sh) — the bin fetches the run's `script` arg and
# executes it. No image of their own.
is_script_entry() { case "$1" in bash | testenv) return 0 ;; *) return 1 ;; esac; }
# std entries whose image is STREAMED to the registry instead of imported into
# git (design/flake-images.md): the nix tarball's layers are composed onto the
# stock base with `docker build` and pushed; the std entry is a tiny curry
# node over the digest ref, so no layer bytes ever enter git. Today that's the
# flake-builder — the one remaining digest-referenced bootstrap image.
is_streamed_entry() { case "$1" in flake-builder) return 0 ;; *) return 1 ;; esac; }
image_names=()
for name in "${names[@]}"; do
  is_flake_entry "$name" || is_script_entry "$name" || image_names+=("$name")
done

# caos-cli: a prebuilt binary if the caller injected one (CAOS_CLI — how caosd
# runs us from a store copy with no `nix` at runtime), else built from the flake.
if [ -n "${CAOS_CLI:-}" ]; then
  caos=$CAOS_CLI
else
  nix build .#caos-cli -o result-caos
  caos=$PROJECT/result-caos/bin/caos-cli
fi
SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
export CAOS_SERVER_URL=$SERVER_URL

# A local client working repo with the server as its `caos` remote — the same
# shape a user has. `caos-cli` builds objects here (in-process via gix); `git
# push` ships them to the server. Reused across runs (git init is idempotent).
# CAOS_CLIENT_REPO relocates it off PROJECT (which is read-only when caosd runs
# us from the store); caosd points it at $CAOS_DATA so it persists per-project.
CLIENT=${CAOS_CLIENT_REPO:-$PROJECT/.caos-dev/client-repo}
git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$SERVER_URL" 2>/dev/null \
  || git -C "$CLIENT" remote set-url caos "$SERVER_URL"

image_attr() { echo "caos-worker-$1-docker"; } # std name -> nix docker image attr

# The image tarball store paths. If the caller prebuilt them (CAOS_BUILTIN_IMAGES,
# a whitespace-separated list — how caosd hands us the flake's images with no
# `nix` at runtime), use those; else build every image in ONE nix invocation (the
# builds run in parallel under a single, low-memory evaluation). Either way, map
# each path back to its builtin via the image name baked into it
# (<hash>-caos-worker-<name>.tar.gz).
if [ -n "${CAOS_BUILTIN_IMAGES:-}" ]; then
  built_paths=$CAOS_BUILTIN_IMAGES
else
  attrs=()
  for name in "${image_names[@]}"; do attrs+=(".#$(image_attr "$name")"); done
  echo "building ${#image_names[@]} images in parallel..." >&2
  if ! built_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
    echo "build-builtins: nix build failed" >&2; exit 1
  fi
fi
declare -A img_path
# Unquoted: word-split on whitespace, covering both nix-build's newline-per-path
# output and a space-separated CAOS_BUILTIN_IMAGES. Store paths never contain
# whitespace or glob chars, so this is safe.
# shellcheck disable=SC2086
for p in $built_paths; do
  for name in "${image_names[@]}"; do
    case "$p" in *-caos-worker-"$name".tar.gz) img_path[$name]=$p ;; esac
  done
done
for name in "${image_names[@]}"; do
  [ -n "${img_path[$name]:-}" ] || { echo "build-builtins: no image built for $name" >&2; exit 1; }
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

declare -A hash_of

# The streamed std entries (design/flake-images.md): compose the nix tarball's
# layers onto the stock nixos/nix base with `docker build` — ADD extracts each
# layer.tar as root, preserving the setuid caos — push the result to the local
# registry, and publish the std entry as a curry node over the digest ref
# (`base` = docker://<host-facing ref>; runnerd's docker pulls it directly).
# The registry tag, keyed on the tarball's immutable store path, is the memo:
# an unchanged image re-publishes with one HEAD request and no build.
REGISTRY=localhost:5000 # the compose stack's registry, host-published (caosd)
manifest_digest() { # <repo:tag> -> the registry's manifest digest, or empty
  curl -fsSI \
    -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
    "http://$REGISTRY/v2/caos/manifests/$1" 2>/dev/null \
    | tr -d '\r' | awk 'tolower($1)=="docker-content-digest:" {print $2}'
}
for name in "${image_names[@]}"; do
  is_streamed_entry "$name" || continue
  tarball=${img_path[$name]}
  stag="$name-$(printf '%s' "$tarball" | sha1sum | cut -c1-12)"
  digest=$(manifest_digest "$stag" || true)
  if [ -n "$digest" ]; then
    echo "$name: registry hit for $stag" >&2
  else
    echo "$name: composing + streaming $stag..." >&2
    ctx="$WORK/stream-$name"
    mkdir "$ctx"
    tar -xzf "$tarball" -C "$ctx"
    cfg=$(jq -r '.[0].Config' "$ctx/manifest.json")
    {
      # The stock base (pinned by digest in images/nix-base.ref) — nix and its
      # store stay stock registry layers, shared with every other consumer.
      printf 'FROM %s\n' "$(cat "$PROJECT/images/nix-base.ref")"
      jq -r '.[0].Layers[] | "ADD \(.) /"' "$ctx/manifest.json"
      jq -r '.config.Env[]? | "ENV \(.)"' "$ctx/$cfg"
      printf 'ENTRYPOINT %s\n' "$(jq -c '.config.Entrypoint' "$ctx/$cfg")"
    } > "$ctx/Dockerfile"
    docker build -t "$REGISTRY/caos:$stag" "$ctx" >&2
    docker push "$REGISTRY/caos:$stag" >&2
    digest=$(manifest_digest "$stag")
    [ -n "$digest" ] || { echo "build-builtins: no digest for pushed $stag" >&2; exit 1; }
  fi
  hash_of[$name]=$(cd "$CLIENT" && "$caos" curry "docker://$REGISTRY/caos@$digest" --)
  echo "$name: streamed -> curry ${hash_of[$name]}" >&2
done

# The flake-tree std entries (design/flake-images.md): std/cargo-base is the
# GENERATED bake tree — the checked-in flake + a lock derived from the main
# flake.lock + the workspace's manifests (stage-tree.sh) — published as a
# plain git tree. The server's flake-builder turns it into the toolchain+deps
# image on first use, memoized in the registry on the tree's own hash, so
# re-publishing an unchanged tree costs nothing. Staged inside CLIENT (like
# worker-common below) because only git-tracked paths can be hashed here.
for name in "${names[@]}"; do
  is_flake_entry "$name" || continue
  rm -rf "${CLIENT:?}/$name"
  "$PROJECT/std/$name/stage-tree.sh" "$PROJECT" "$CLIENT/$name"
  git -C "$CLIENT" add "$name"
  hash_of[$name]=$(git -C "$CLIENT" write-tree --prefix="$name/")
  echo "$name: flake tree ${hash_of[$name]}" >&2
done

# The script workers: std/bash and std/testenv are curry(<name>-base,
# bin=images/bash-worker.sh) — the same runner-pool move as the binary
# workers below, with the script runner as the bin. The bin fetches the run's
# `script` arg and executes it with bash; callers curry their script on top
# exactly as before. Staged in CLIENT (ingestion takes git-tracked paths).
for name in "${names[@]}"; do
  is_script_entry "$name" || continue
  base="$name-base"
  # A name-scoped run may not have staged this worker's base tree; skip.
  [ -n "${hash_of[$base]:-}" ] || continue
  install -m 755 "$PROJECT/images/bash-worker.sh" "$CLIENT/bash-worker.sh"
  git -C "$CLIENT" add bash-worker.sh
  hash_of[$name]=$(cd "$CLIENT" && "$caos" curry "${hash_of[$base]}" -- "--worker1:@=bash-worker.sh")
  echo "$name: script curry ${hash_of[$name]}" >&2
done

# Worker binaries: each is published as
# a ready-to-run curry over the shared runner image — std/<name> =
# curry(runner, bin=<static binary>) — NOT as a worker image of its own, so its
# runs ride the warm runner pool (design/runner-protocol.md) and a rebuild
# ships one small blob, not an image. `caos-cli curry` ingests the binary and
# pushes the curry; the std ref push below pins both. Prebuilt store paths
# arrive via CAOS_BUILTIN_BINS (how caosd avoids runtime nix), else they're
# nix-built here. Skipped when `runner` isn't among the names (a partial,
# name-scoped run has no image to curry onto). Most bins curry onto the shared
# runner; `cargo` curries onto its toolchain base (bin_base — the same move at
# a different base: the heavy, rarely-changing image is keyed on
# toolchain+lockfile, and a worker rebuild ships one blob).
# Order matters: rustc's curry references the published cargo worker, so
# cargo precedes it. The example workers (hello, file-count, dirs-only,
# deep-deps) ride the same mechanism — their dedicated images are gone.
bin_names=(bash-tool llm-step rgrep cargo rustc hello file-count dirs-only deep-deps)
bin_base() { # worker binary -> the image its std curry binds it into
  case "$1" in
    cargo) echo "cargo-base" ;;
    *) echo "runner" ;;
  esac
}
if [ -n "${hash_of[runner]:-}" ]; then
  if [ -n "${CAOS_BUILTIN_BINS:-}" ]; then
    bin_paths=$CAOS_BUILTIN_BINS
  else
    attrs=()
    for b in "${bin_names[@]}"; do attrs+=(".#worker-$b"); done
    echo "building ${#bin_names[@]} worker binaries..." >&2
    if ! bin_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
      echo "build-builtins: nix build failed" >&2; exit 1
    fi
  fi
  declare -A bin_path
  # Map each worker binary to the store path that CONTAINS it (bin/worker-<b>),
  # not by matching the path's name: the workspace builds as one derivation
  # (caos-workspace), so every worker binary shares a single store path — a
  # name match on "-worker-<b>" would never hit. Existence of bin/worker-<b>
  # is the honest test and works whether bins are one path or many.
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    for b in "${bin_names[@]}"; do
      [ -x "$p/bin/worker-$b" ] && bin_path[$b]=$p
    done
  done
  for b in "${bin_names[@]}"; do
    [ -n "${bin_path[$b]:-}" ] || { echo "build-builtins: no binary built for worker-$b" >&2; exit 1; }
    base=$(bin_base "$b")
    # A name-scoped run may not have imported this bin's base image; skip.
    [ -n "${hash_of[$base]:-}" ] || continue
    # rustc is orchestration over the cargo worker: curry in the published
    # cargo curry (as a literal image ref) and the worker-common source tree
    # its generated projects link against.
    extra=()
    if [ "$b" = rustc ]; then
      [ -n "${hash_of[cargo]:-}" ] || continue
      rm -rf "$CLIENT/worker-common"
      cp -R "$PROJECT/crates/worker-common" "$CLIENT/worker-common"
      # PROJECT may be a read-only store copy (caosd); writable so the next
      # publish's rm -rf works.
      chmod -R u+w "$CLIENT/worker-common"
      git -C "$CLIENT" add worker-common
      extra=("--cargo=${hash_of[cargo]}" "--worker_common:@=worker-common")
    fi
    # Ingestion only accepts git-tracked worktree paths, so stage a copy of
    # the binary in the client repo (overwritten on every publish).
    install -m 755 "${bin_path[$b]}/bin/worker-$b" "$CLIENT/worker-$b"
    git -C "$CLIENT" add "worker-$b"
    hash_of[$b]=$(cd "$CLIENT" && "$caos" curry "${hash_of[$base]}" -- "--worker1:@=worker-$b" "${extra[@]}")
    echo "$b: curry ${hash_of[$b]}" >&2
    names+=("$b")
  done
fi

# Assemble the {name: image} tree (a ref can name any object; std is a tree, so
# there's no commit to wrap it) and publish it to the server under refs/caos/std
# in one push, which uploads every builtin image the server doesn't already have.
entries=""
for name in "${names[@]}"; do
  entries+="040000 tree ${hash_of[$name]}"$'\t'"$name"$'\n'
done
tree=$(printf '%s' "$entries" | git -C "$CLIENT" mktree)
# --force: refs/caos/std points at a tree, and git refuses to update a non-commit
# ref (or move it) without it. Re-publishing always replaces it.
git -C "$CLIENT" push -q --force caos "$tree:refs/caos/std"
# Record it locally too, so this repo can also resolve refs/caos/std.
git -C "$CLIENT" update-ref refs/caos/std "$tree"
echo "refs/caos/std -> $tree (published to $SERVER_URL)" >&2
echo "$tree"
