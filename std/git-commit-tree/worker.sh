#!/usr/bin/env bash
set -euo pipefail

for arg in tree parents author committer message; do
  caos get "/cas/args/$arg"
done
args=("$(cat /cas/args/tree)")
read -r -a parents <<< "$(cat /cas/args/parents)"
for parent in "${parents[@]}"; do
  args+=("--parent=$parent")
done
args+=("--author=$(cat /cas/args/author)" "--committer=$(cat /cas/args/committer)" "--message-file=/cas/args/message")
caos git-commit-tree "${args[@]}" > /tmp/commit-result
caos put /tmp/commit-result /cas/out
