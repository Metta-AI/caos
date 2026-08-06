#!/bin/bash
# Runs *inside* a bash worker, in the `run` position: fails, loudly and
# deterministically. The fixture for `--catch` — the sub-run whose failure the
# continuation either swallows into `--error` or propagates, depending on the
# flag.
set -euo pipefail
echo "boom: the run step failed on purpose" >&2
exit 1
