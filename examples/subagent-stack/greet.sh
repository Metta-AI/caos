#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -gt 1 ]; then
  echo "usage: greet.sh [name]" >&2
  exit 2
fi
printf 'Hello, %s!\n' "${1:-world}"
