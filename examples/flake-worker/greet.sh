#!/bin/sh
# The worker binary, curried in as `bin` and exec'd by the flake-builder's
# runner trampoline (design/flake-images.md). caos is at /bin/caos (the caos
# delta stacked on this flake's base image).
echo "hello from a flake-built worker (curried bin via the trampoline)" > /tmp/out
/bin/caos put /tmp/out /cas/out
