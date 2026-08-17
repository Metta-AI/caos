#!/usr/bin/env bash
set -euo pipefail

q=$(caos hash /cas/args)
echo "EXACT_FAILED_REQUEST=$q" >&2
echo "subrequest failed deliberately" >&2
exit 23
