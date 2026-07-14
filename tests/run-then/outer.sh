#!/bin/bash
# Runs *inside* a bash worker, in the `run` position — and its own result is
# another run-then (over the curried image in --inner-img). So the driver's
# `run` sub-run returns a promise, which the server must resolve to a value
# before the driver's `then` sees it as --result.
set -euo pipefail
caos get /cas/args/inner-img
caos run-then /cas/args/in -- --run="$(cat /cas/args/inner-img)"
