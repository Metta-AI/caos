#!/usr/bin/env bash
# Populate the caos `std` library — the workers clients reach as
# `/cas/std/<name>` — and publish it to the server as `refs/caos/std`.
#
# Entries come in three forms (see the is_*_entry predicates below):
#   streamed  (flake-builder, runner)    host-nix-built core, composed with
#                                        `docker build` and pushed to the
#                                        registry; the entry is a curry over
#                                        the digest ref
#   flake     (cargo, bash, testenv)     literal checked-in std/<name> dirs —
#                                        complete worker images, /worker
#                                        included (cargo compiles its own
#                                        in-flake); the flake-builder images
#                                        each tree on first use
#   curry     (bin_names)                curry(runner, worker1=<binary>) — the
#                                        compiled workers ride the shared
#                                        runner pool
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
[ ${#names[@]} -eq 0 ] && names=(runner cargo bash testenv flake-builder)

# std entries that are FLAKE TREES (design/flake-images.md, part 2): the
# checked-in std/<name> directory IS the published tree, copied whole —
# nothing generated (std/refresh.sh maintains the checked-in redundancies,
# tests/std-lint verifies them). The server's flake-builder images each
# tree on first use.
is_flake_entry() { case "$1" in cargo | bash | testenv) return 0 ;; *) return 1 ;; esac; }
# std entries whose image is STREAMED to the registry instead of imported into
# git (design/flake-images.md): host-nix-built core, composed with `docker
# build` and pushed; the std entry is a tiny curry node over the digest ref,
# so no layer bytes ever enter git. The flake-builder (the bootstrap image,
# composed onto stock nixos/nix) and the runner (the pooled interpreter —
# self-contained, FROM scratch).
is_streamed_entry() { case "$1" in flake-builder | runner) return 0 ;; *) return 1 ;; esac; }
image_names=()
for name in "${names[@]}"; do
  is_flake_entry "$name" || image_names+=("$name")
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

# The worker binaries published as curries over runner (bin_names below).
# Prebuilt store paths arrive via CAOS_BUILTIN_BINS (how caosd avoids
# runtime nix), else they're nix-built here. Nothing is staged into flake
# trees anymore: runner's /worker bakes into its streamed image, cargo's
# compiles in-flake from the vendored source.
bin_names=(bash-tool llm-step rgrep rustc)
if [ -n "${CAOS_BUILTIN_BINS:-}" ]; then
  bin_paths=$CAOS_BUILTIN_BINS
else
  attrs=()
  for b in "${bin_names[@]}"; do attrs+=(".#worker-$b"); done
  echo "building worker binaries..." >&2
  if ! bin_paths=$(nix build "${attrs[@]}" --no-link --print-out-paths); then
    echo "build-builtins: nix build failed" >&2; exit 1
  fi
fi

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
    # The flake-builder composes onto stock nixos/nix (pinned by digest in
    # std/flake-builder/base.ref) so nix and its store stay stock registry layers,
    # shared with every other consumer. The runner is self-contained — its
    # nix-built userland IS the image — so it builds FROM scratch.
    case "$name" in
    flake-builder) from=$(cat "$PROJECT/std/flake-builder/base.ref") ;;
    runner) from=scratch ;;
    *) echo "build-builtins: no stream base for $name" >&2; exit 1 ;;
    esac
    {
      printf 'FROM %s\n' "$from"
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

# The flake-tree std entries (design/flake-images.md): bash/testenv are
# LITERAL — the checked-in std/<name> directory is the published tree,
# copied whole; runner/cargo stage their nix-built /worker binary onto the
# checked-in files (std/<name>/stage-tree.sh). Either way the server's
# flake-builder images the tree on first use, memoized in the registry on
# the tree's own hash, so re-publishing an unchanged tree costs nothing.
# Staged inside CLIENT (like worker-common below) because only git-tracked
# paths can be hashed here.
for name in "${names[@]}"; do
  is_flake_entry "$name" || continue
  rm -rf "${CLIENT:?}/$name"
  cp -R "$PROJECT/std/$name" "$CLIENT/$name"
  # PROJECT may be a read-only store copy (caosd); writable so the next
  # publish's rm -rf works.
  chmod -R u+w "$CLIENT/$name"
  git -C "$CLIENT" add "$name"
  hash_of[$name]=$(git -C "$CLIENT" write-tree --prefix="$name/")
  echo "$name: flake tree ${hash_of[$name]}" >&2
done

# The compiled workers: each is published as a ready-to-run curry over the
# shared runner image — std/<name> = curry(runner, worker1=<static binary>) —
# NOT as a worker image of its own, so its runs ride the warm runner pool
# (design/runner-protocol.md) and a rebuild ships one small blob, not an
# image. `caos-cli curry` ingests the binary and pushes the curry; the std
# ref push below pins both. Skipped when `runner` isn't among the names (a
# partial, name-scoped run has no image to curry onto).
if [ -n "${hash_of[runner]:-}" ]; then
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
    # rustc is orchestration over the cargo worker: curry in the published
    # cargo entry (as a literal image ref) and the worker-common source tree
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
    hash_of[$b]=$(cd "$CLIENT" && "$caos" curry "${hash_of[runner]}" -- "--worker1:@=worker-$b" "${extra[@]}")
    echo "$b: curry ${hash_of[$b]}" >&2
    names+=("$b")
  done
fi

# The full nix-built binary set, published as refs/caos/bins — what the
# build/test tools (caos-tools/*.sh) consume instead of recompiling the
# workspace in-caos: run-tool resolves this ref and passes its hash as
# --bins. Canonical runtime names; kept OUT of the std tree so binary
# churn (a server edit) never re-keys std consumers. The workspace builds
# as one derivation, so any built path's bin/ carries every binary —
# existence is the honest lookup, same as bin_path above. Staged in
# CLIENT like everything else (only git-tracked paths can be hashed).
rm -rf "${CLIENT:?}/bins"
mkdir "$CLIENT/bins"
stage_tool_bin() { # <name>
  local p
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    if [ -x "$p/bin/$1" ]; then
      install -m 755 "$p/bin/$1" "$CLIENT/bins/$1"
      return 0
    fi
  done
  echo "build-builtins: no binary $1 in the built paths" >&2
  exit 1
}
for n in caos caos-cli server runnerd llm-stub; do stage_tool_bin "$n"; done
# shellcheck disable=SC2086
for p in $bin_paths; do
  for w in "$p"/bin/worker-*; do
    [ -e "$w" ] && install -m 755 "$w" "$CLIENT/bins/$(basename "$w")"
  done
done
git -C "$CLIENT" add bins
bins_tree=$(git -C "$CLIENT" write-tree --prefix=bins/)
git -C "$CLIENT" push -q --force caos "$bins_tree:refs/caos/bins"
git -C "$CLIENT" update-ref refs/caos/bins "$bins_tree"
echo "refs/caos/bins -> $bins_tree (published to $SERVER_URL)" >&2

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
