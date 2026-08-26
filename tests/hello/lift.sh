#!/bin/bash
# Runs *inside* a bash worker, as the fixture's `outer/.caos-expr`: produce a
# tree whose `tool` entry IS the hello entry, so that `outer/tool` names
# something no directory on disk holds.
set -euo pipefail
caos get /cas/args/src
mkdir -p /tmp/lifted
# By REFERENCE: `caos put` resolves the symlink to the recorded hash, so the
# entry is renamed, not copied.
ln -s /cas/args/src/hello /tmp/lifted/tool
caos put /tmp/lifted /cas/out
