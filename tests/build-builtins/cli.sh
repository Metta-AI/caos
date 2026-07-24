#!/usr/bin/env bash
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
tree_with_blob() {
  local repo=$1 name=$2 contents=$3 blob
  blob=$(printf '%s' "$contents" | git -C "$repo" hash-object -w --stdin)
  printf '100644 blob %s\t%s\n' "$blob" "$name" | git -C "$repo" mktree
}

work=$(mktemp -d)
remote=$work/remote.git
client=$work/client
seed=$work/seed
image=$work/fake-caos-worker-file-count.tar.gz
git init -q --bare "$remote"
git init -q "$seed"

old=$(tree_with_blob "$seed" old old)
bash_tree=$(tree_with_blob "$seed" bash bash)
std=$(printf '040000 tree %s\tbash\n040000 tree %s\tfile-count\n' \
  "$bash_tree" "$old" | git -C "$seed" mktree)
git -C "$seed" remote add origin "$remote"
git -C "$seed" push -q origin "$std:refs/caos/std"

git init -q "$client"
new=$(tree_with_blob "$client" new new)
touch "$image"
source_ref="refs/caos/src/$(printf '%s' "$image" | sha1sum | cut -c1-40)"
git -C "$client" update-ref "$source_ref" "$new"

CAOS_CLIENT_REPO=$client \
CAOS_SERVER_URL=$remote \
CAOS_CLI=/bin/false \
CAOS_BUILTIN_IMAGES=$image \
  bash "$CAOS_BUILD_BUILTINS" file-count >/dev/null

published=$(git --git-dir="$remote" ls-tree refs/caos/std)
grep -q "$bash_tree[[:space:]]bash$" <<<"$published" ||
  fail "partial publish removed the existing bash entry"
grep -q "$new[[:space:]]file-count$" <<<"$published" ||
  fail "partial publish did not update file-count"

empty_remote=$work/empty.git
empty_client=$work/empty-client
git init -q --bare "$empty_remote"
git init -q "$empty_client"
if CAOS_CLIENT_REPO=$empty_client \
  CAOS_SERVER_URL=$empty_remote \
  CAOS_CLI=/bin/false \
  CAOS_BUILTIN_IMAGES=$image \
    bash "$CAOS_BUILD_BUILTINS" file-count >"$work/out" 2>&1; then
  fail "partial publish bootstrapped an incomplete std ref"
fi
grep -q "partial publish requires an existing refs/caos/std" "$work/out" ||
  fail "partial bootstrap did not explain why it was rejected"

echo "build-builtins: ALL PASS"
