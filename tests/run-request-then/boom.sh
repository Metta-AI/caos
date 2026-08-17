#!/usr/bin/env bash
set -euo pipefail

q=$(caos hash /cas/args)
echo "EXACT_FAIL_REQUEST=$q" >&2
exit 23
