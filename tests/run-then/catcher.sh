#!/bin/bash
# Runs *inside* a bash worker, in the `then` position of a `--catch`
# continuation: the run step FAILED, so the server binds --error (a blob of the
# failure text) where --result would have been. Prove the original --in still
# arrived alongside it, and that --result did not.
set -euo pipefail
caos get /cas/args/in
if [ -e /cas/args/result ]; then
  echo "catcher: --result is bound on a caught failure" >&2
  exit 1
fi
caos get /cas/args/error
# Echoed for the record; the assertion is only that SOMETHING arrived. The
# wording of a worker's failure is not this test's business to pin.
echo "catcher saw --error:" >&2
cat /cas/args/error >&2
if [ -s /cas/args/error ]; then caught=yes; else caught=empty; fi
printf 'in=%s caught=%s' "$(cat /cas/args/in)" "$caught" > /tmp/caught
caos put /tmp/caught /cas/out
