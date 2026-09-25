#!/usr/bin/env bash
set -euo pipefail

for arg in tree parents author committer message; do
  caos get "/cas/args/$arg"
done
tree=$(cat /cas/args/tree)
read -r -a parents <<< "$(cat /cas/args/parents)"
for oid in "$tree" "${parents[@]}"; do
  if [[ ! "$oid" =~ ^[0-9a-fA-F]{40}$ ]]; then
    echo "expected a full object hash" >&2
    exit 1
  fi
done
author=$(cat /cas/args/author)
committer=$(cat /cas/args/committer)
for signature in "$author" "$committer"; do
  if [[ "$signature" == *$'\n'* || "$signature" == *$'\r'* ]]; then
    echo "signature must fit on one line" >&2
    exit 1
  fi
done
{
  printf 'tree %s\n' "$tree"
  for parent in "${parents[@]}"; do printf 'parent %s\n' "$parent"; done
  printf 'author %s\ncommitter %s\n\n' "$author" "$committer"
  cat /cas/args/message
} > /tmp/commit
# put-commit validates the raw commit and its dependencies.
caos put-commit /tmp/commit /cas/commit-result > /tmp/commit-result
caos put /tmp/commit-result /cas/out
