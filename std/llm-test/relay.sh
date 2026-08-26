#!/bin/bash
# Runs *inside* a bash worker, dispatched by the test with `sub-run`. Its whole
# body is a tail call: run the curried llm-call request in `--call`.
#
# WHY THIS EXISTS — it is not indirection for its own sake. A secret is injected
# only when the job's ArgTree already carries the matching `secret-hash`
# (design/secrets.md, fail-closed), and the ONLY thing that folds that entry in
# server-side is `run_image`, the dispatch behind `run-then`/`map-then`. A
# worker's own `prepare-request` forms an ArgTree with an EMPTY store
# (`caos_prepare_request` passes `&[]`), so a request the test hands straight to
# `sub-run` can never be granted anything.
#
# But `run-then` is a CONTINUATION: the caller records it and exits — and the
# test cannot exit, because it is hosting the stub the call has to reach. So the
# run-then happens one job down. `sub-run` carries the grant list into this job
# (`start_sub_run` clones `secrets`), this job's `run-then` folds the hash for
# llm-call, and the test stays alive serving the stub.
set -euo pipefail
caos get /cas/args/call
caos run-then /cas/args/call --run:hash="$(cat /cas/args/call)"
