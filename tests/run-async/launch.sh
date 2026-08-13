#!/usr/bin/env bash
set -euo pipefail

caos get /cas/args/request
reply=$(caos run-async "$(< /cas/args/request)")
printf '%s\n' "$reply" > /tmp/admitted
caos put /tmp/admitted /cas/out
