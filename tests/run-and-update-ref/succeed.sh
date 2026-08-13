#!/usr/bin/env bash
set -euo pipefail

q=$(caos hash /cas/args)
mkdir /tmp/exact-result
printf 'payload from R\n' > /tmp/exact-result/payload
printf '%s\n' "$q" > /tmp/exact-result/request
caos put /tmp/exact-result /cas/out
