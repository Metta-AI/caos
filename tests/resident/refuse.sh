#!/usr/bin/env bash
# Runs `caos next` where it must be refused, and reports what it said. Which
# refusal depends on the image and the job it is curried onto:
#   std/bash               the image does not declare CAOS_RESIDENT=1
#   resident, no affinity  the job names no instance to stay resident for
set -euo pipefail

if caos next 2>/tmp/err; then
  echo "caos next was accepted" > /tmp/out
else
  printf 'refused: %s' "$(cat /tmp/err)" > /tmp/out
fi
caos put /tmp/out /cas/out
