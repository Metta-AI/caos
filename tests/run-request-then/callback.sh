#!/usr/bin/env bash
set -euo pipefail

if [ -e /cas/args/in ]; then
  echo "exact-request callback unexpectedly received --in" >&2
  exit 1
fi

if [ -e /cas/args/result ]; then
  if [ -e /cas/args/error ]; then
    echo "callback received both --result and --error" >&2
    exit 1
  fi
  caos get /cas/args/result
  printf 'result=%s\n' "$(cat /cas/args/result)" > /tmp/callback
elif [ -e /cas/args/error ]; then
  caos get /cas/args/error
  grep -q "exit status: 23" /cas/args/error || {
    echo "caught error did not contain the worker failure" >&2
    exit 1
  }
  printf 'caught=yes\n' > /tmp/callback
else
  echo "callback received neither --result nor --error" >&2
  exit 1
fi

caos put /tmp/callback /cas/out
