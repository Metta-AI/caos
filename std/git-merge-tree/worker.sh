#!/usr/bin/env bash
set -euo pipefail

for arg in merge-base ours theirs; do
  caos get "/cas/args/$arg"
done
caos git-merge-tree "$(cat /cas/args/merge-base)" "$(cat /cas/args/ours)" "$(cat /cas/args/theirs)" > /tmp/merge-result
caos put /tmp/merge-result /cas/out
