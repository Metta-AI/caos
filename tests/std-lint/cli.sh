#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# The literal-tree lints (design/flake-images.md, part 2): std's checked-in
# redundancies must match what std/refresh.sh regenerates from their sources
# of truth — each std flake.lock re-derives byte-identically from the root
# flake.lock.
# The check IS the generator run in --check mode (one script, one code
# path), so the lint cannot drift from what a refresh would write. The
# workspace arrives in this test's wrapper, staged as $CAOS_PROJECT
# (caos-tools/test.sh, the `fanout` stage). Fast: no compiles, no caos jobs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== std/refresh.sh --check: every checked-in std copy re-derives ==" >&2
bash "$CAOS_PROJECT/std/refresh.sh" --check || fail "checked-in std copies are stale"

# The other literal-tree lint, and the only check in this suite that covers
# `nix build`. Everything else compiles the tree with cargo over the real
# crates/ directory, so the flake's `src` filter — which hands rustc a FILTERED
# copy — is exercised by nothing. A file it drops is invisible to a green run;
# that is exactly how the githist/*.sh embeds landed. Same shape as the lint
# above: it re-derives the rule rather than restating a result.
echo "== lint-flake-src.sh: every embedded file survives the flake's src filter ==" >&2
bash "$CAOS_PROJECT/lint-flake-src.sh" "$CAOS_PROJECT" \
  || fail "a file the crates compile from is dropped by the flake's src filter"

echo "== lint-bake-anchor.sh: every std tool's crates.io deps are anchored ==" >&2
bash "$CAOS_PROJECT/lint-bake-anchor.sh" \
  || fail "a std tool's crates.io dep is missing from bake-anchor (see above)"

echo "== image-cleanup.sh: seed roots, age, and LRU size bound ==" >&2
fixture=$(mktemp -d)
repo="$fixture/data/publish-client-repo"
mkdir -p "$repo"
git init -q "$repo"
seed_digest=sha256:$(printf 'a%.0s' {1..64})
stale_digest=sha256:$(printf 'b%.0s' {1..64})
recent_digest=sha256:$(printf 'c%.0s' {1..64})
base_blob=$(printf 'docker://localhost:5000/caos@%s' "$seed_digest" \
  | git -C "$repo" hash-object -w --stdin)
result_tree=$(printf '100644 blob %s\tbase\n' "$base_blob" | git -C "$repo" mktree)
record_tree=$(printf '040000 tree %s\tresult\n' "$result_tree" | git -C "$repo" mktree)
seed_tree=$(printf '040000 tree %s\trunner\n' "$record_tree" | git -C "$repo" mktree)
git -C "$repo" update-ref refs/caos/seed "$seed_tree"
revision_root="$fixture/data/stack/registry/docker/registry/v2/repositories/caos/_manifests/revisions/sha256"
tag_root="$fixture/data/stack/registry/docker/registry/v2/repositories/caos/_manifests/tags"
mkdir -p "$revision_root/${seed_digest#sha256:}" \
  "$revision_root/${stale_digest#sha256:}" \
  "$revision_root/${recent_digest#sha256:}" \
  "$tag_root/caos-used-${stale_digest#sha256:}/current" \
  "$tag_root/caos-used-${recent_digest#sha256:}/current"
printf '%s' "$seed_digest" > "$revision_root/${seed_digest#sha256:}/link"
printf '%s' "$stale_digest" > "$revision_root/${stale_digest#sha256:}/link"
printf '%s' "$recent_digest" > "$revision_root/${recent_digest#sha256:}/link"
printf '%s' "$stale_digest" > "$tag_root/caos-used-${stale_digest#sha256:}/current/link"
printf '%s' "$recent_digest" > "$tag_root/caos-used-${recent_digest#sha256:}/current/link"
touch -t 197001010000.01 "$tag_root/caos-used-${stale_digest#sha256:}/current/link"
touch -t 197001121346.40 "$tag_root/caos-used-${recent_digest#sha256:}/current/link"
blob_root="$fixture/data/stack/registry/docker/registry/v2/blobs/sha256"
for digest in "$seed_digest" "$stale_digest" "$recent_digest"; do
  hex=${digest#sha256:}
  mkdir -p "$blob_root/${hex:0:2}/$hex"
  printf '{"schemaVersion":2,"layers":[]}' > "$blob_root/${hex:0:2}/$hex/data"
done
cleanup_out=$(
  CAOS_DATA="$fixture/data" CAOS_IMAGE_CLEANUP_NOW=1000000 \
    CAOS_IMAGE_CLEANUP_MAX_BYTES=40 \
    bash "$CAOS_PROJECT/image-cleanup.sh" --unused-for=7d
) || fail "image cleanup dry run failed"
printf '%s\n' "$cleanup_out" | grep -q "KEEP    $seed_digest  current seed (runner)" \
  || fail "the current seed was not retained"
printf '%s\n' "$cleanup_out" | grep -q "DELETE  $stale_digest" \
  || fail "the stale non-seed manifest was not selected"
printf '%s\n' "$cleanup_out" | grep -q "DELETE  $recent_digest  LRU over" \
  || fail "the recent manifest beyond the size ceiling was not selected"

echo "std-lint: ALL PASS" >&2
