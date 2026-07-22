#!/bin/bash
# Build final (the `then` of the cargo-image build): --result is the cargo
# worker image's digest ref. Assemble THE ARTIFACT TREE — everything the
# tree's build produces:
#   report                a deterministic one-line summary (run-tool prints it)
#   bin/<name>            the workspace binaries (content-stable)
#   images/{runner,bash,cargo}   registry-digest refs
# Symlinks + `caos put`: recorded-hash reuse, no bytes move.
set -euo pipefail

caos get /cas/args/bin
caos get /cas/args/result

mkdir -p /tmp/art/images
ln -s /cas/args/bin /tmp/art/bin
ln -s /cas/args/runner_image /tmp/art/images/runner
ln -s /cas/args/bash_image /tmp/art/images/bash
ln -s /cas/args/result /tmp/art/images/cargo

nbins=$(ls /cas/args/bin | wc -l)
printf 'BUILD OK: %s binaries, 3 images\n' "$nbins" > /tmp/art/report
caos put /tmp/art /cas/out
