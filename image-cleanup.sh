#!/usr/bin/env bash
# Bound CAOS's two image caches. Docker is only the local working set, so every
# unused CAOS image there is disposable. The registry is the backing cache: it
# keeps the most recently used manifest closures up to a size ceiling, plus the
# current std seed regardless of age or size.
set -euo pipefail
export LC_ALL=C

: "${CAOS_DATA:?image-cleanup needs CAOS_DATA}"

case "$CAOS_DATA" in
  /|"")
    echo "caosd image-cleanup: refusing unsafe CAOS_DATA '$CAOS_DATA'" >&2
    exit 1
    ;;
esac

STATE="$CAOS_DATA/stack"
CLIENT="$CAOS_DATA/publish-client-repo"
REPOSITORY=caos
USED_TAG_PREFIX=caos-used-
DEFAULT_UNUSED_FOR=7d
DEFAULT_MAX_SIZE=20GiB
GIB=$((1024 * 1024 * 1024))

execute=no
unused_for=$DEFAULT_UNUSED_FOR
max_size=$DEFAULT_MAX_SIZE

usage() {
  echo "usage: caosd image-cleanup [--unused-for=<days>d] [--max-size=<gib>GiB] [--execute]" >&2
}

fail() {
  echo "caosd image-cleanup: $*" >&2
  exit 1
}

mtime() {
  if stat -c %Y "$1" 2>/dev/null; then
    return
  fi
  stat -f %m "$1"
}

format_epoch() {
  if date -u -d "@$1" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null; then
    return
  fi
  date -u -r "$1" '+%Y-%m-%dT%H:%M:%SZ'
}

format_bytes() {
  local bytes=$1 whole tenth
  whole=$((bytes / GIB))
  tenth=$(((bytes % GIB) * 10 / GIB))
  printf '%d.%dGiB' "$whole" "$tenth"
}

for arg in "$@"; do
  case "$arg" in
    --execute)
      execute=yes
      ;;
    --unused-for=*)
      unused_for=${arg#--unused-for=}
      ;;
    --max-size=*)
      max_size=${arg#--max-size=}
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      fail "unknown argument '$arg'"
      ;;
  esac
done

if [[ ! "$unused_for" =~ ^[0-9]+d$ ]]; then
  fail "--unused-for must be a whole number of days such as 7d"
fi
unused_days=${unused_for%d}
if [ "$unused_days" -lt 1 ]; then
  fail "--unused-for must be at least 1d"
fi
if [[ ! "$max_size" =~ ^[0-9]+GiB$ ]]; then
  fail "--max-size must be whole GiB such as 20GiB"
fi
max_gib=${max_size%GiB}
if [ "$max_gib" -lt 1 ]; then
  fail "--max-size must be at least 1GiB"
fi
max_bytes=${CAOS_IMAGE_CLEANUP_MAX_BYTES:-$((max_gib * GIB))}
if [[ ! "$max_bytes" =~ ^[0-9]+$ ]] || [ "$max_bytes" -lt 1 ]; then
  fail "CAOS_IMAGE_CLEANUP_MAX_BYTES must be positive bytes"
fi
budget=$(format_bytes "$max_bytes")

[ -d "$STATE/registry" ] || fail "no registry under $STATE (run 'caosd up' first)"
[ -d "$CLIENT" ] || fail "no publish client under $CLIENT (run 'caosd up' first)"

manifest_root="$STATE/registry/docker/registry/v2/repositories/$REPOSITORY/_manifests"
revision_root="$manifest_root/revisions/sha256"
tag_root="$manifest_root/tags"
blob_root="$STATE/registry/docker/registry/v2/blobs/sha256"
[ -d "$revision_root" ] || fail "registry has no $REPOSITORY manifest revisions"

scratch=$(mktemp -d "$STATE/image-cleanup.XXXXXX")
inventory="$scratch/inventory"
candidates="$scratch/candidates"
candidate_report="$scratch/candidate-report"
retained="$scratch/retained"
: > "$inventory"
: > "$candidates"
: > "$candidate_report"
: > "$retained"

registry_container=""
gc_container=""
stack_stopped=no
finish() {
  local rc=$?
  trap - EXIT
  if [ -n "$registry_container" ] && docker container inspect "$registry_container" >/dev/null 2>&1; then
    docker container stop "$registry_container" >/dev/null 2>&1 || true
    docker container rm "$registry_container" >/dev/null 2>&1 || true
  fi
  if [ -n "$gc_container" ] && docker container inspect "$gc_container" >/dev/null 2>&1; then
    docker container stop "$gc_container" >/dev/null 2>&1 || true
    docker container rm "$gc_container" >/dev/null 2>&1 || true
  fi
  rm -rf "$scratch"
  if [ "$stack_stopped" = yes ]; then
    if docker container start caos-stack >/dev/null 2>&1; then
      ready=""
      for _ in $(seq 1 60); do
        if curl -s -o /dev/null --max-time 2 http://localhost:9090/; then
          ready=yes
          break
        fi
        sleep 1
      done
      if [ -n "$ready" ]; then
        echo "caosd image-cleanup: restarted caos-stack"
      else
        echo "caosd image-cleanup: caos-stack did not become ready after restart" >&2
        rc=1
      fi
    else
      echo "caosd image-cleanup: could not restart caos-stack" >&2
      rc=1
    fi
  fi
  exit "$rc"
}
trap finish EXIT

declare -A protected=()
seed=$(git -C "$CLIENT" rev-parse --verify -q refs/caos/seed) \
  || fail "no refs/caos/seed (run 'caosd up' first)"
protected_count=0
while read -r _mode _kind oid name; do
  base=$(git -C "$CLIENT" cat-file -p "$oid:result/base" 2>/dev/null) || continue
  digest=${base##*@}
  if [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] && [ -z "${protected[$digest]:-}" ]; then
    protected[$digest]=$name
    protected_count=$((protected_count + 1))
  fi
done < <(git -C "$CLIENT" ls-tree "$seed")
[ "$protected_count" -gt 0 ] || fail "the current std seed names no registry manifests"

now=${CAOS_IMAGE_CLEANUP_NOW:-$(date +%s)}
if [[ ! "$now" =~ ^[0-9]+$ ]]; then
  fail "CAOS_IMAGE_CLEANUP_NOW must be Unix seconds"
fi
cutoff=$((now - unused_days * 86400))
manifest_count=0
untracked_count=0

while IFS= read -r -d '' revision_link; do
  digest_hex=${revision_link%/link}
  digest_hex=${digest_hex##*/}
  digest="sha256:$digest_hex"
  recorded=$(<"$revision_link")
  [ "$recorded" = "$digest" ] || fail "manifest revision $revision_link records $recorded"
  manifest_count=$((manifest_count + 1))

  marker="$tag_root/$USED_TAG_PREFIX$digest_hex/current/link"
  if [ -n "${protected[$digest]:-}" ]; then
    printf 'seed\t%s\t%s\t%s\n' "$now" "$digest" "${protected[$digest]}" >> "$inventory"
  elif [ -f "$marker" ]; then
    marker_digest=$(<"$marker")
    [ "$marker_digest" = "$digest" ] \
      || fail "usage marker $marker records $marker_digest, expected $digest"
    printf 'tracked\t%s\t%s\t-\n' "$(mtime "$marker")" "$digest" >> "$inventory"
  else
    # No creation timestamp is trustworthy for reproducible images. Treat a
    # legacy manifest as used now; --execute gives it a durable marker.
    printf 'untracked\t%s\t%s\t-\n' "$now" "$digest" >> "$inventory"
    untracked_count=$((untracked_count + 1))
  fi
done < <(find "$revision_root" -mindepth 2 -maxdepth 2 -type f -name link -print0)

blob_path() {
  local digest=$1 hex=${1#sha256:}
  [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid registry blob digest $digest"
  printf '%s/%s/%s/data\n' "$blob_root" "${hex:0:2}" "$hex"
}

declare -A kept_blobs=()
closure_digests=()
closure_increment=0
plan_closure() {
  local digest=$1 manifest_blob blob path size
  manifest_blob=$(blob_path "$digest")
  [ -f "$manifest_blob" ] || fail "manifest blob $digest is missing"
  jq -r '(.config.digest? // empty), (.layers[]?.digest // empty)' \
    "$manifest_blob" > "$scratch/descriptors" \
    || fail "manifest blob $digest is not valid JSON"
  {
    printf '%s\n' "$digest"
    while IFS= read -r blob; do printf '%s\n' "$blob"; done < "$scratch/descriptors"
  } | sort -u > "$scratch/closure" \
    || fail "sorting the blob closure for $digest"
  closure_digests=()
  while IFS= read -r blob; do
    [ -n "$blob" ] || continue
    closure_digests+=("$blob")
  done < "$scratch/closure"
  closure_increment=0
  for blob in "${closure_digests[@]}"; do
    if [ -n "${kept_blobs[$blob]:-}" ]; then
      continue
    fi
    path=$(blob_path "$blob")
    [ -f "$path" ] || fail "blob $blob referenced by $digest is missing"
    size=$(stat -c %s "$path" 2>/dev/null || stat -f %z "$path")
    closure_increment=$((closure_increment + size))
  done
}

retain_planned_closure() {
  local blob
  for blob in "${closure_digests[@]}"; do
    kept_blobs[$blob]=1
  done
}

# Roots enter first and may exceed the ceiling: correctness beats the cache
# budget. Everything else is considered newest-first, yielding a true LRU over
# the union of manifest/config/layer blobs rather than double-counting layers.
retained_bytes=0
while IFS=$'\t' read -r _kind _used digest detail; do
  plan_closure "$digest"
  retain_planned_closure
  retained_bytes=$((retained_bytes + closure_increment))
  printf '%s\n' "$digest" >> "$retained"
  printf '  KEEP    %s  current seed (%s)\n' "$digest" "$detail"
done < <(grep '^seed' "$inventory" || true)
roots_over_budget=no
if [ "$retained_bytes" -gt "$max_bytes" ]; then
  roots_over_budget=yes
fi

candidate_count=0
while IFS=$'\t' read -r kind used digest _detail; do
  if [ "$used" -lt "$cutoff" ]; then
    printf '%s\n' "$digest" >> "$candidates"
    printf 'age\t%s\t%s\n' "$used" "$digest" >> "$candidate_report"
    candidate_count=$((candidate_count + 1))
    continue
  fi

  plan_closure "$digest"
  if [ $((retained_bytes + closure_increment)) -gt "$max_bytes" ]; then
    printf '%s\n' "$digest" >> "$candidates"
    printf 'size\t%s\t%s\n' "$used" "$digest" >> "$candidate_report"
    candidate_count=$((candidate_count + 1))
    continue
  fi
  retain_planned_closure
  retained_bytes=$((retained_bytes + closure_increment))
  printf '%s\n' "$digest" >> "$retained"
done < <(grep -v '^seed' "$inventory" | sort -t $'\t' -k2,2nr -k3,3 || true)

echo "caosd image-cleanup: $manifest_count manifests; $protected_count seed roots; $candidate_count selected; $untracked_count without history"
echo "caosd image-cleanup: planned registry content $(format_bytes "$retained_bytes") / $budget; age limit $unused_for"
if [ "$roots_over_budget" = yes ]; then
  echo "caosd image-cleanup: seed roots alone exceed the registry budget" >&2
fi
while IFS=$'\t' read -r reason used digest; do
  stamp=$(format_epoch "$used")
  case "$reason" in
    age)  printf '  DELETE  %s  last used %s\n' "$digest" "$stamp" ;;
    size) printf '  DELETE  %s  LRU over %s (last used %s)\n' "$digest" "$budget" "$stamp" ;;
  esac
done < "$candidate_report"

if [ "$execute" = no ]; then
  echo "caosd image-cleanup: dry run; pass --execute to apply"
  exit 0
fi

# Stop accepting work before touching either cache. A worker already running is
# left alone; the caller can retry once it finishes. With no worker containers,
# a test stack is idle and safe to remove.
running=$(docker ps --format '{{.Names}}' | grep -E '^caos-worker-' || true)
if [ -n "$running" ]; then
  echo "caosd image-cleanup: active workers:" >&2
  while IFS= read -r name; do echo "  $name" >&2; done <<< "$running"
  fail "wait for active work to finish before --execute"
fi

if docker container inspect caos-registry-cleanup >/dev/null 2>&1 \
  || docker container inspect caos-registry-gc >/dev/null 2>&1; then
  fail "a previous cleanup container remains; inspect and remove it first"
fi

if [ "$(docker inspect -f '{{.State.Running}}' caos-stack 2>/dev/null || true)" = true ]; then
  docker container stop caos-stack >/dev/null
  stack_stopped=yes
fi

running=$(docker ps --format '{{.Names}}' | grep -E '^caos-worker-' || true)
[ -z "$running" ] || fail "a worker started while the stack was stopping; retry cleanup"

while IFS= read -r container_id; do
  [ -n "$container_id" ] || continue
  docker container rm -f "$container_id" >/dev/null
done < <(docker ps -aq --filter name=caos-test-stack-)

if [ "$candidate_count" -gt 0 ] || [ "$untracked_count" -gt 0 ]; then
  config="$scratch/registry.yml"
  cat > "$config" <<YML
version: 0.1
storage:
  delete:
    enabled: true
  filesystem:
    rootdirectory: /state/registry
http:
  addr: :5000
YML
  config_in_container="/state/${scratch##*/}/registry.yml"

  registry_container=caos-registry-cleanup
  docker run -d --name "$registry_container" \
    -p 5000:5000 \
    -v "$STATE:/state" \
    --entrypoint /bin/registry \
    caos-stack:latest serve "$config_in_container" >/dev/null

  ready=""
  for _ in $(seq 1 30); do
    if curl -fs -o /dev/null http://localhost:5000/v2/; then
      ready=yes
      break
    fi
    sleep 1
  done
  [ -n "$ready" ] || fail "temporary registry did not become ready"

  accept='application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
  while IFS= read -r digest; do
    digest_hex=${digest#sha256:}
    marker="$tag_root/$USED_TAG_PREFIX$digest_hex/current/link"
    if [ -f "$marker" ]; then
      continue
    fi
    curl -fsS -D "$scratch/headers" -o "$scratch/manifest" \
      -H "Accept: $accept" \
      "http://localhost:5000/v2/$REPOSITORY/manifests/$digest"
    content_type=""
    while IFS= read -r header; do
      header=${header%$'\r'}
      case "$header" in
        Content-Type:*|content-type:*)
          content_type=${header#*:}
          content_type=${content_type# }
          ;;
      esac
    done < "$scratch/headers"
    [ -n "$content_type" ] || fail "manifest $digest response has no Content-Type"
    curl -fsS -o /dev/null -X PUT \
      -H "Content-Type: $content_type" \
      --data-binary @"$scratch/manifest" \
      "http://localhost:5000/v2/$REPOSITORY/manifests/$USED_TAG_PREFIX$digest_hex"
  done < "$retained"

  while IFS= read -r digest; do
    code=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
      "http://localhost:5000/v2/$REPOSITORY/manifests/$digest")
    case "$code" in
      202|404) ;;
      *) fail "deleting $digest returned HTTP $code" ;;
    esac
  done < "$candidates"

  docker container stop "$registry_container" >/dev/null
  docker container rm "$registry_container" >/dev/null
  registry_container=""

  if [ "$candidate_count" -gt 0 ]; then
    # A cached result can contain a now-deleted registry digest indirectly, so
    # invalidate the cache only when deletion actually happened.
    rm -rf "$STATE/redis"
    mkdir -p "$STATE/redis"

    gc_container=caos-registry-gc
    if ! docker run --name "$gc_container" \
      -v "$STATE:/state" \
      --entrypoint /bin/registry \
      caos-stack:latest garbage-collect --quiet --delete-untagged "$config_in_container" \
      > "$scratch/gc.log" 2>&1; then
      tail -n 30 "$scratch/gc.log" >&2
      fail "registry garbage collection failed"
    fi
    docker container rm "$gc_container" >/dev/null
    gc_container=""
  fi
fi

# Docker is a working set in front of the local registry. Once CAOS is idle,
# every pulled CAOS image can go; a later run restores it from the registry.
docker_removed=0
while IFS= read -r image_id; do
  [ -n "$image_id" ] || continue
  local_caos=""
  while IFS= read -r reference; do
    case "$reference" in
      localhost:5000/caos@sha256:*|caos-registry:5000/caos@sha256:*)
        local_caos=yes
        break
        ;;
    esac
  done < <(docker image inspect -f '{{range .RepoDigests}}{{println .}}{{end}}' "$image_id")
  if [ -n "$local_caos" ] && docker image rm "$image_id" >/dev/null 2>&1; then
    docker_removed=$((docker_removed + 1))
  fi
done < <(docker image ls -q --no-trunc | sort -u)

# load_once's content tags avoid repeated docker loads, but only the tag for the
# current stack image is useful.
current_stack=$(docker image inspect -f '{{.Id}}' caos-stack:latest 2>/dev/null || true)
while read -r tag image_id; do
  case "$tag" in
    caos-stack-src:*)
      if [ "$image_id" != "$current_stack" ]; then
        docker image rm "$tag" >/dev/null 2>&1 || true
      fi
      ;;
  esac
done < <(docker image ls --no-trunc --format '{{.Repository}}:{{.Tag}} {{.ID}}')

registry_bytes=$(du -sk "$STATE/registry" | while read -r kib _rest; do echo $((kib * 1024)); done)
echo "caosd image-cleanup: deleted $candidate_count registry manifests; removed $docker_removed local CAOS images"
echo "caosd image-cleanup: registry now uses $(format_bytes "$registry_bytes")"
