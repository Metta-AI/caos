#!/usr/bin/env bash
set -euo pipefail

for name in worker subreq target-ref; do
  caos get "/cas/args/$name"
done

q=$(caos prepare-request "$(< /cas/args/worker)" -- \
  --subreq="$(< /cas/args/subreq)" \
  --target-ref="$(< /cas/args/target-ref)")
[ "${#q}" -eq 40 ] && [[ "$q" =~ ^[0-9a-f]+$ ]]
printf '%s\n' "$q" > /tmp/prepared-request
caos put /tmp/prepared-request /cas/out
