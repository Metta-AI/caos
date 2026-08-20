#!/usr/bin/env bash
set -euo pipefail

q=$(caos hash /cas/args)
# A blocking implementation would wait on this worker's own result. Bound the
# regression so a broken test fails promptly instead of hanging the suite.
reply=$(timeout 5 caos sub-run "$q")

[ "$reply" = "request $q" ] || {
  echo "sub-run returned $reply, expected request $q" >&2
  exit 1
}

printf '%s\n' "$reply" > /tmp/reply
caos put /tmp/reply /cas/out
