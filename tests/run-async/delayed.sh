#!/usr/bin/env bash
set -euo pipefail

caos get /cas/args/payload
sleep 2
cp /cas/args/payload /tmp/background-result
caos put /tmp/background-result /cas/out
