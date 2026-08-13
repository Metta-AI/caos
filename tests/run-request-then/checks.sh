#!/usr/bin/env bash
set -euo pipefail

caos get /cas/args/request
request=$(cat /cas/args/request)

if caos run-request-then not-a-hash -- 2>/tmp/err; then
  echo "run-request-then accepted a malformed request" >&2
  exit 1
fi
grep -q "needs a 40-character ArgTree hash" /tmp/err || {
  echo "wrong malformed-request error: $(cat /tmp/err)" >&2
  exit 1
}

if caos run-request-then "$request" -- --catch 2>/tmp/err; then
  echo "run-request-then accepted --catch without --then" >&2
  exit 1
fi
grep -q -- "--catch.*needs --then" /tmp/err || {
  echo "wrong catch-without-then error: $(cat /tmp/err)" >&2
  exit 1
}

if caos run-request-then "$request" -- --run="$request" 2>/tmp/err; then
  echo "run-request-then accepted --run" >&2
  exit 1
fi
grep -q "takes only --then" /tmp/err || {
  echo "wrong unsupported-option error: $(cat /tmp/err)" >&2
  exit 1
}

printf 'ok\n' > /tmp/ok
caos put /tmp/ok /cas/out
