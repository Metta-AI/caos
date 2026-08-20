#!/usr/bin/env bash
# Bootstrap the SEEDED CORE: build the handful of images that cannot be built by
# the machinery they are, and publish them as seed records under `refs/caos/seed`.
#
# It publishes NO std library: every `std/<name>` is a checked-in source
# directory whose `.caos-expr` says how it is built, and a consumer reaches it by
# DESCENT — a `DEPS` line naming a path, expanded by a root `.caos-expr` into a
# `DEEP-DEPS/<name>` mount (design/caos-expr.md). Nothing resolves a builtin by
# an ambient name, so do not add a ref for one.
#
# The irreducible core: five entries name a `docker://seeded…` sentinel instead
# of a real builder, because each would otherwise have to build itself:
#
#   flake-builder  a flake built by the flake-builder
#   cargo          a flake, so likewise
#   runner         the pooled interpreter: a nix closure with no source here
#   rustc          the worker factory; rustc compiling rustc
#   deep-deps      the transform a tree needs before it can be deepened
#
# For each, this script hand-builds the artifact its expression WOULD have
# produced and records it as `{ required: <the arg-tree key>, result: <what to
# answer with> }`. The core-seeder-runner registers one poll per record and
# answers that exact key, spawning no container. `required` is assembled from the
# deltas already built here rather than by running the evaluator, so no seeder has
# to be live during bootstrap.
#
# The keys must match what resolution forms, which is why the deepen pass below
# exists: a seeded item's `in` is its DEEPENED entry, and this has to know that
# hash before any stack exists to compute it. It must agree with the deep-deps
# worker BYTE FOR BYTE (tests/deep-deps and the suite are the guardrail).
#
# Usage: ./build-builtins.sh [name ...]   (default: all)
# Requires the dev server running and git + skopeo + curl on PATH. No docker:
# this script composes no images — it pushes clean ones and describes deltas.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

names=("$@")
if [ ${#names[@]} -eq 0 ]; then
  names=(runner cargo bash flake-builder git-runner merge rgrep bash-tool llm-client llm-call llm-step run-and-update-ref deep-deps rustc llm-stub)
fi

# Which entries have a HOST-BUILT nix image behind them. This is the whole
# partition: everything else is a checked-in source directory this script
# only reads (to compute a seed key), never builds.
#
#   flake-builder  the bootstrap image
#   runner         the pooled interpreter
#   cargo          built by the root flake from the same `src` and toolchain as
#                  the binaries, so its deps are cargoArtifacts rather than a
#                  second compile of them
#
has_host_image() {
  case "$1" in
    flake-builder | runner | cargo) return 0 ;;
    *) return 1 ;;
  esac
}
image_names=()
for name in "${names[@]}"; do
  if has_host_image "$name"; then image_names+=("$name"); fi
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
# When the caller says this repo dies with the process (caos-tools/build.sh runs
# us against a /tmp client inside the build container), turn zlib OFF. Bootstrap
# writes ~67 MB of freshly-compiled binaries as loose objects and then pushes
# ~13 MB of pack to a server that was created empty seconds ago, and deflating
# binaries at level 1 was measured as most of that push. A persistent client repo
# (caosd's, under $CAOS_DATA) must NOT get this: there the objects are kept.
if [ "${CAOS_CLIENT_REPO_THROWAWAY:-no}" = yes ]; then
  git -C "$CLIENT" config core.compression 0
  git -C "$CLIENT" config pack.compression 0
fi
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
# trees: runner's /worker bakes into its streamed image, cargo's
# compiles in-flake from the vendored source.
bin_names=(deep-deps rustc)
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

# A phase clock, into the SAME file the calling stage's `ts` writes
# (caos-tools/build.sh), so this script's phases interleave with the build's in
# the one report. Off unless the caller names a file: on the host this script
# runs interactively and its stderr is the report.
bts() { if [ -n "${CAOS_PHASES:-}" ]; then echo "$(date +%s%3N) builtins: $*" >> "$CAOS_PHASES"; fi; }
bts "start"

declare -A hash_of

REGISTRY=localhost:5000 # the compose stack's registry, host-published (caosd)
# The same registry as THIS SCRIPT reaches it over HTTP. On the host the two
# names coincide. Inside the test stack (design/test-stack-image.md) they do
# not: we run in a container on caos-net and must call it caos-registry:5000.
REGISTRY_HTTP=${CAOS_REGISTRY_HTTP:-$REGISTRY}
# How the SERVER reaches the same registry: it pulls a delta's `base` with
# skopeo from inside its own container, where the registry is a service on
# the docker network. Mirrors the server's own CAOS_REGISTRY_PUSH_URL default.
REGISTRY_BASE_HOST=${CAOS_REGISTRY_BASE_HOST:-caos-registry:5000}
# <repo:tag> -> the registry's manifest digest, or empty when the tag is absent.
# ABSENT AND BROKEN ARE DIFFERENT: a 404 is the ordinary first-push case and
# returns empty, but an unreachable registry or any other status is fatal —
# swallowing those would silently re-push (or worse, mint a `base` ref for an
# image nothing can pull).
manifest_digest() { # <repo:tag>
  local url headers status
  url="http://$REGISTRY_HTTP/v2/caos/manifests/$1"
  headers=$(curl -sSI \
    -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
    "$url") || { echo "build-builtins: registry unreachable: $url" >&2; exit 1; }
  status=$(printf '%s\n' "$headers" | awk 'NR==1 {print $2}')
  case "$status" in
    200) printf '%s\n' "$headers" \
           | tr -d '\r' \
           | awk 'tolower($1)=="docker-content-digest:" {print $2}' ;;
    404) ;;
    *) echo "build-builtins: registry returned $status for $url" >&2; exit 1 ;;
  esac
}
tool_bin() { # <name> -> the path of bin/<name> among the built paths
  local p
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    if [ -x "$p/bin/$1" ]; then
      echo "$p/bin/$1"
      return 0
    fi
  done
  echo "build-builtins: no binary $1 in the built paths" >&2
  exit 1
}
caos_bin=$(tool_bin caos)

# Which workspace binary is a streamed image's `/worker`. These images are
# HOST-BUILT and the host is their author, so their interpreter is composed on
# here rather than baked into the nix image — the clean image then depends on
# nothing in the workspace and is pushed once, keyed on its store path alone,
# while the binary rides as a layer keyed on its BYTES. flake-builder is
# absent because its /worker comes from its own flake, not from this
# workspace.
worker_bin_of() { # <std name> -> the workspace binary that is its /worker, or empty
  case "$1" in
    runner) echo worker-runner ;;
    cargo) echo worker-cargo ;;
    *) ;;
  esac
}

# The caos additions, staged as a git-docker LAYER TREE — the same shape
# std/flake-builder's stack stage emits, so both image paths in this tree
# compose deltas exactly one way. The server rebuilds the layer tar from
# this at convert time and applies the `<name>.caosmeta` sidecars for the
# modes git cannot record (setuid, sticky).
#
# Staged inside CLIENT because only git-tracked paths can be hashed here.
ADD=$CLIENT/layer-additions
rm -rf "${ADD:?}"
mkdir -p "$ADD/bin" "$ADD/usr/bin" "$ADD/etc"
install -m 755 "$caos_bin" "$ADD/bin/caos"
# setuid needs no privilege here: the sidecar is data, and the SERVER (root)
# applies the mode when it rebuilds the layer.
printf '{"mode":"4755","uid":0,"gid":0}' > "$ADD/bin/caos.caosmeta"
# /usr/bin/env for env-shebang scripts; every streamed userland has /bin/env.
ln -s /bin/env "$ADD/usr/bin/env"
printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' \
  > "$ADD/etc/passwd"
printf 'root:x:0:\nworker:x:1000:\n' > "$ADD/etc/group"
# The world-writable /tmp. Its 1777 mode rides in a sidecar because git records
# no modes beyond the exec bit, and the DIRECTORY is carried by a keep-file
# because it must not be EMPTY.
#
# An empty directory is a legal git tree entry, but it does not survive a
# materialize -> read -> re-put round trip, so any worker that rebuilds a tree
# from the filesystem silently drops it — deep-deps is one, and a dropped
# `layer00/tmp` makes the worker-deepened entry disagree with the hand-deepen
# below, which breaks every seeded key that reaches the runner. Do not replace
# the keep-file with an empty tree.
mkdir -p "$ADD/tmp"
printf 'This file exists so that /tmp is a NON-EMPTY directory.\n\nAn empty directory is not representable in a git worktree and does not survive\na materialize -> read -> re-put round trip, so a worker that rebuilds a tree\nfrom the filesystem (deep-deps) drops it. /tmp must exist in the image, so it\ncarries this file. Its 1777 mode rides in the tmp.caosmeta sidecar.\n' \
  > "$ADD/tmp/.caos-keep"
printf '{"mode":"1777","uid":0,"gid":0}' > "$ADD/tmp.caosmeta"
git -C "$CLIENT" add layer-additions
# `tmp` is an ordinary directory in the staged layout, so `write-tree` carries
# it like everything else.
additions_tree=$(git -C "$CLIENT" write-tree --prefix=layer-additions/)

# Each streamed image's /worker, its own layer. worker-runner is the pool
# protocol's in-image half; worker-cargo is what the cargo image is FOR — and
# neither belongs in the nix closure it runs on. The cargo image is 3.4 GB of
# toolchain and baked deps, so baking a 5 MB binary into it made every Rust
# edit re-tar and re-gzip all of it under `nix build` and then gunzip and
# re-push it under `caosd up` (design/flake-images.md listed exactly this as
# the open boundary-caching item).
declare -A worker_tree
for name in "${image_names[@]}"; do
  wb=$(worker_bin_of "$name")
  if [ -n "$wb" ]; then
    WD=$CLIENT/layer-worker-$name
    rm -rf "${WD:?}"
    mkdir -p "$WD"
    install -m 755 "$(tool_bin "$wb")" "$WD/worker"
    git -C "$CLIENT" add "layer-worker-$name"
    worker_tree[$name]=$(git -C "$CLIENT" write-tree --prefix="layer-worker-$name/")
  fi
done
bts "staged worker layers"

# The streamed std entries (design/one-stack-image.md): the nix tarball is the
# CLEAN image — no caos, no user db, no /tmp — and it goes to the registry
# ONCE, keyed on its store path ALONE. The clean images depend on nothing in
# the workspace, so a Rust edit never re-pushes one.
#
# The std entry itself is a git-docker delta {base, config.json, layer<NN>}:
# the server stacks our layers on that base and pushes the result the first
# time something runs it, memoized in redis by the tree's hash. So no layer
# bytes of the CLEAN image ever enter git, and this script does no image
# composition at all — no docker, no build context, no manifest surgery.
for name in "${image_names[@]}"; do
  tarball=${img_path[$name]}
  ctag="clean-$name-$(printf '%s' "$tarball" | sha1sum | cut -c1-12)"
  digest=$(manifest_digest "$ctag")
  if [ -n "$digest" ]; then
    echo "$name: registry hit for $ctag" >&2
  else
    echo "$name: streaming clean image $ctag..." >&2
    # skopeo's docker-archive transport reads an uncompressed tar; nix hands
    # us .tar.gz.
    gunzip -c "$tarball" > "$WORK/$name.tar"
    skopeo --insecure-policy copy --dest-tls-verify=false \
      "docker-archive:$WORK/$name.tar" "docker://$REGISTRY_HTTP/caos:$ctag" >&2
    rm -f "$WORK/$name.tar"
    digest=$(manifest_digest "$ctag")
    [ -n "$digest" ] || { echo "build-builtins: no digest for pushed $ctag" >&2; exit 1; }
  fi

  img=$CLIENT/img-$name
  rm -rf "${img:?}"
  mkdir -p "$img"
  # `base` is read by the SERVER, which pulls it with skopeo from inside its
  # own container (crates/server/src/compute.rs, fetch_base) — so it carries
  # the on-network registry name, not the host-published one the docker
  # daemon uses.
  printf 'docker://%s/caos@%s' "$REGISTRY_BASE_HOST" "$digest" > "$img/base"
  # The clean image's own OCI config, verbatim: it already names the runner
  # entrypoint and its PATH, because its flake said so.
  skopeo --insecure-policy inspect --tls-verify=false --config \
    "docker://$REGISTRY_HTTP/caos:$ctag" > "$img/config.json"
  git -C "$CLIENT" add "img-$name"
  hash_of[$name]=$(
    {
      git -C "$CLIENT" ls-tree "$(git -C "$CLIENT" write-tree --prefix="img-$name/")"
      printf '040000 tree %s\tlayer00\n' "$additions_tree"
      # Not `[ ... ] && printf`: this is the block's last command, so under
      # `set -e -o pipefail` a false test would fail the whole substitution
      # for every entry that carries no /worker of its own.
      if [ -n "${worker_tree[$name]:-}" ]; then
        printf '040000 tree %s\tlayer01\n' "${worker_tree[$name]}"
      fi
    } | git -C "$CLIENT" mktree
  )
  echo "$name: git-docker delta ${hash_of[$name]} over $ctag" >&2
  bts "delta for $name"
done

# ---- the hand-deepen: SEED KEYS ONLY, never published -----------------------
# A seeded core item's arg-tree `in` is its DEEPENED entry — what a root
# `.caos-expr` produces — and bootstrap must know that hash before any stack
# exists to compute it. So it hand-deepens here, and the hand-deepen must match
# the deep-deps worker BYTE FOR BYTE or the seeder registers a key no caller ever
# forms. Only `cargo` has DEPS among the seeded core (the rest are DEPS-free, so
# their deepened form is identity), which is where that identity bites.

# The image loop above left git-docker deltas in hash_of[]. Capture them before
# the deepen pass overwrites those entries with their source trees: each delta is
# a SEED RESULT.
runner_delta=${hash_of[runner]:-}
cargo_delta=${hash_of[cargo]:-}
fb_delta=${hash_of[flake-builder]:-}

# Map each worker binary to the store path that CONTAINS it (bin/worker-<b>) —
# every worker binary shares one workspace store path, so existence of
# bin/worker-<b> is the honest test.
declare -A bin_path
if [ -n "$runner_delta" ]; then
  # shellcheck disable=SC2086
  for p in $bin_paths; do
    for b in "${bin_names[@]}"; do
      if [ -x "$p/bin/worker-$b" ]; then
        bin_path[$b]=$p
      fi
    done
  done
fi

declare -A undeepened staged_sources

# Stage a checked-in source dir into CLIENT and record its tree by dependency
# name. Most are std/<name>; a DEPS path may point at source elsewhere.
# `cp -RL`: PROJECT may be an image layout of SYMLINKS into the nix store (the
# test stack); dereference so what is published is the content.
stage_source() { # <name> <source-dir>
  local name=$1 source_dir=$2
  rm -rf "${CLIENT:?}/$name"
  cp -RL "$source_dir" "$CLIENT/$name"
  chmod -R u+w "$CLIENT/$name" # PROJECT may be a read-only store copy (caosd)
  git -C "$CLIENT" add "$name"
  undeepened[$name]=$(git -C "$CLIENT" write-tree --prefix="$name/")
  staged_sources[$name]=1
}

# DEPS may also name source that is not itself a builtin. Its tree still
# participates in the caller's deepened cache key, so bootstrap must stage it
# before deepen_entry recurses into it. Follow paths from the declaring source
# directory, like deep-deps, instead of maintaining a second name list.
stage_source_closure() { # <name> <source-dir>
  local name=$1 source_dir=$2 dep_path mount
  if [ -n "${staged_sources[$name]:-}" ]; then return; fi
  stage_source "$name" "$source_dir"
  if [ -f "$source_dir/DEPS" ]; then
    while read -r dep_path mount; do
      if [ -n "$dep_path" ]; then
        case "$dep_path" in
          \#*) ;;
          *) stage_source_closure "$(basename "$dep_path")" "$source_dir/$dep_path" ;;
        esac
      fi
    done < "$source_dir/DEPS"
  fi
}

for name in "${names[@]}"; do
  # EVERY entry is a checked-in source dir, `runner` included: `std/rustc/DEPS`
  # says `../runner`, and a tree cannot be self-resolving when a declared
  # dependency is not IN it. Publishing runner as a bare delta would leave
  # `std/runner` a name with no directory behind it; it is a `{.caos-expr}`
  # sentinel, seeded like flake-builder (below).
  stage_source_closure "$name" "$PROJECT/std/$name"
done
bts "staged ${#staged_sources[@]} local sources"

# The hand-deepen — SEED KEYS ONLY, never published (see the block header).
# deepen_entry(name) replaces the entry's top-level DEPS with a DEEP-DEPS/
# subtree of its deepened local dependencies, recursively and memoized — so a
# dep reached twice is ONE shared tree (git dedups the identical hash, matching
# the deep-deps worker's
# absolute-path sharing). An entry with no DEPS is identity (its subtrees are
# DEPS-free, which the worker reproduces unchanged — so this matches byte for
# byte). Divergence from the worker is not silent: the seeded key stops matching
# the key resolution forms, the job falls through to the generic runner and dies
# on a sentinel image, and every test that resolves the item goes red.
declare -A deepened
deepen_entry() { # <name> -> deepened tree hash (stdout)
  local name=$1
  if [ -n "${deepened[$name]:-}" ]; then echo "${deepened[$name]}"; return; fi
  local tree=${undeepened[$name]} deps_oid="" meta file
  while IFS=$'\t' read -r meta file; do
    if [ "$file" = DEPS ]; then deps_oid=${meta##* }; fi
  done < <(git -C "$CLIENT" ls-tree "$tree")

  local result
  if [ -z "$deps_oid" ]; then
    result=$tree
  else
    # DEEP-DEPS/<mount> = deepened(declared path). DEPS lines are
    # `<path> <name>`; comments/blank lines skipped. mktree sorts.
    local dd_lines="" dep_path mount
    while read -r dep_path mount; do
      if [ -n "$dep_path" ]; then
        case "$dep_path" in
          \#*) ;;
          *) dd_lines+="040000 tree $(deepen_entry "$(basename "$dep_path")")"$'\t'"$mount"$'\n' ;;
        esac
      fi
    done < <(git -C "$CLIENT" cat-file blob "$deps_oid")
    local dd_tree
    dd_tree=$(printf '%s' "$dd_lines" | git -C "$CLIENT" mktree)
    # The entry's own tree minus DEPS, plus the DEEP-DEPS subtree.
    result=$(
      {
        git -C "$CLIENT" ls-tree "$tree" | while IFS=$'\t' read -r meta file; do
          if [ "$file" != DEPS ]; then printf '%s\t%s\n' "$meta" "$file"; fi
        done
        printf '040000 tree %s\tDEEP-DEPS\n' "$dd_tree"
      } | git -C "$CLIENT" mktree
    )
  fi
  deepened[$name]=$result
  echo "$result"
}

for name in "${names[@]}"; do
  hash_of[$name]=$(deepen_entry "$name")
  echo "$name: deepened entry (seed key) ${hash_of[$name]}" >&2
done
bts "hand-deepened ${#names[@]} entries"

# What a caller's `--in:@=.` actually resolves to: the entry MINUS its own
# `.caos-expr`. eval-path hands an expression its directory excluding the
# directive (crates/caos/src/eval.rs `strip_caos_expr`), so every seeded item —
# each of whose expressions is `run --base:<t>=<builder> --in:@=.` — forms an `in` that
# is the stripped tree. Strip here too or the seeder registers a key no caller
# ever forms, and the job falls through to the generic runner and dies on a
# sentinel image.
#
# For the sentinel entries (`runner`, `rustc`, `deep-deps`) the whole directory
# IS the `.caos-expr`, so this is the empty tree — honest: there is no source,
# which is why they are seeded. Their sentinels differ, so their keys still do.
strip_caos_expr() { # <tree> -> tree hash (stdout)
  git -C "$CLIENT" ls-tree "$1" | while IFS=$'\t' read -r meta file; do
    if [ "$file" != .caos-expr ]; then printf '%s\t%s\n' "$meta" "$file"; fi
  done | git -C "$CLIENT" mktree
}

declare -A in_of
for name in "${names[@]}"; do
  in_of[$name]=$(strip_caos_expr "${hash_of[$name]}")
done

# ---- seed records (design/caos-expr.md, Phase 3) ----------------------------
# The irreducible core can't be built by the machinery it IS, so bootstrap
# hand-builds each core artifact and publishes a SEED RECORD per item under
# refs/caos/seed; the core-seeder-runner registers one poll per record and
# answers the arg-tree key a caller forms, spawning no container.
#
#   refs/caos/seed -> tree { <name> -> { required: <JSON name→oid>, result: <obj> } }
#
# bootstrap HAND-ASSEMBLES each `required` from the deltas it already built —
# NOT by running the evaluator — so it needs no live answerer and no seeder
# during publish (the deltas are known; the key a caller's eval forms is
# reproduced here). `required` OMITS `std`/`salt`: a core item's output depends
# only on its `image`+`in`, so the seeder answers under ANY std — which is what
# lets the test harness hand each job a std SUBSET. The server matches a required
# set as a SUBSET of the job's arg entries (runner::matches), still far more
# specific than runnerd's empty required.
seed_entries=""
add_seed_record() { # <name> <required-json> <result-tree>
  local reqblob record
  reqblob=$(printf '%s' "$2" | git -C "$CLIENT" hash-object -w --stdin)
  record=$(printf '100644 blob %s\trequired\n040000 tree %s\tresult\n' \
    "$reqblob" "$3" | git -C "$CLIENT" mktree)
  seed_entries+="040000 tree $record"$'\t'"$1"$'\n'
  echo "seed: $1 -> $3" >&2
}

if [ -n "$fb_delta" ] && [ -n "${hash_of[flake-builder]:-}" ]; then
  # flake-builder: `run --base:docker=seeded --in:@=.`. The `base` a caller forms
  # for a `docker://` ref is a BLOB naming it (base_arg_entry stores the bytes),
  # so required `base` is that blob's oid; `in` is the (deepened, but DEPS-free
  # so identity) flake-builder source entry.
  seeded_blob=$(printf 'docker://seeded' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record flake-builder \
    "$(printf '{"base":"%s","in":"%s"}' "$seeded_blob" "${in_of[flake-builder]}")" "$fb_delta"
fi

if [ -n "$cargo_delta" ] && [ -n "${hash_of[cargo]:-}" ] && [ -n "$fb_delta" ]; then
  # cargo: `run --base:@=DEEP-DEPS/flake-builder --in:@=.`. Resolving the mount yields
  # the flake-builder image (fb_delta), so the caller's arg-tree `image` is that
  # tree oid and `in` is the deepened cargo entry — both known here.
  add_seed_record cargo \
    "$(printf '{"base":"%s","in":"%s"}' "$fb_delta" "${in_of[cargo]}")" "$cargo_delta"
fi

# rustc/deep-deps: SEEDED core. Their `.caos-expr` is a distinct sentinel
# (`run --base:docker=seeded-{rustc,deep-deps} --in:@=.`), so the caller's key is
# `{ image: <blob of that sentinel>, in: <the {.caos-expr} entry> }`. The RESULT
# is the hand-built curry over the runner pool: a binary bound onto the runner,
# which is what the entry would have resolved to if it could build itself.
if [ -n "$runner_delta" ] && [ -n "${bin_path[deep-deps]:-}" ] && [ -n "${hash_of[deep-deps]:-}" ]; then
  install -m 755 "${bin_path[deep-deps]}/bin/worker-deep-deps" "$CLIENT/seed-deep-deps"
  git -C "$CLIENT" add seed-deep-deps
  bts "staged deep-deps seed inputs"
  dd_curry=$(cd "$CLIENT" && "$caos" curry \
    "--base:hash=$runner_delta" "--worker1:@=seed-deep-deps")
  bts "curried deep-deps"
  dd_blob=$(printf 'docker://seeded-deep-deps' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record deep-deps \
    "$(printf '{"base":"%s","in":"%s"}' "$dd_blob" "${in_of[deep-deps]}")" "$dd_curry"
fi

if [ -n "$runner_delta" ] && [ -n "$cargo_delta" ] && [ -n "${bin_path[rustc]:-}" ] && [ -n "${hash_of[rustc]:-}" ]; then
  install -m 755 "${bin_path[rustc]}/bin/worker-rustc" "$CLIENT/seed-rustc"
  git -C "$CLIENT" add seed-rustc
  rm -rf "${CLIENT:?}/seed-rustc-wc"
  cp -RL "$PROJECT/crates/worker-common" "$CLIENT/seed-rustc-wc"
  chmod -R u+w "$CLIENT/seed-rustc-wc"
  git -C "$CLIENT" add seed-rustc-wc
  # curry(runner, worker1=<rustc bin>, cargo=<cargo image, a literal hash blob>,
  # runner=<the same, for what rustc CURRIES ONTO>, worker-common=<tree>) — the
  # worker factory's ready-to-run form.
  #
  # `--runner` is bootstrap REALIZING std/rustc/DEPS: rustc depends on the runner
  # pool and curries every built binary onto it, so no caller passes one. A hash
  # literal like `--cargo`, because that is what a seed result can bind — when
  # rustc stops being seeded its expression binds DEEP-DEPS/runner instead.
  bts "staged rustc seed inputs"
  rustc_curry=$(cd "$CLIENT" && "$caos" curry \
    "--base:hash=$runner_delta" \
    "--worker1:@=seed-rustc" "--cargo=$cargo_delta" "--runner=$runner_delta" \
    "--worker-common:@=seed-rustc-wc")
  bts "curried rustc"
  rustc_blob=$(printf 'docker://seeded-rustc' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record rustc \
    "$(printf '{"base":"%s","in":"%s"}' "$rustc_blob" "${in_of[rustc]}")" "$rustc_curry"
fi

# runner: the pooled interpreter, a self-contained nix closure with no source to
# build from — so it is seeded exactly like flake-builder. Its checked-in entry
# is the `docker://seeded-runner` sentinel and the host-built delta is the seed
# RESULT. This is what gives `std/rustc/DEPS`'s `../runner` a directory to point
# at, and so what lets the tree deepen itself.
if [ -n "$runner_delta" ] && [ -n "${hash_of[runner]:-}" ]; then
  runner_blob=$(printf 'docker://seeded-runner' | git -C "$CLIENT" hash-object -w --stdin)
  add_seed_record runner \
    "$(printf '{"base":"%s","in":"%s"}' "$runner_blob" "${in_of[runner]}")" "$runner_delta"
fi

if [ -n "$seed_entries" ]; then
  seed_tree=$(printf '%s' "$seed_entries" | git -C "$CLIENT" mktree)
  bts "mktree"
  git -C "$CLIENT" push -q --force caos "$seed_tree:refs/caos/seed"
  git -C "$CLIENT" update-ref refs/caos/seed "$seed_tree"
  echo "refs/caos/seed -> $seed_tree" >&2
fi
bts "pushed seed records"

echo "$seed_tree"
