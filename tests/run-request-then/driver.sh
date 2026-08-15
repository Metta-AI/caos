#!/usr/bin/env bash
set -euo pipefail

caos get /cas/args/request
args=()
if [ -e /cas/args/then-img ]; then
  caos get /cas/args/then-img
  args+=("--then=$(cat /cas/args/then-img)")
fi
if [ -e /cas/args/catch ]; then
  args+=(--catch)
fi

caos run-request-then "$(cat /cas/args/request)" -- "${args[@]}"
