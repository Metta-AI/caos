#!/usr/bin/env bash
# One transition of std/actor-check: apply a message to a state with the inner
# actor. A map-then `map`, so it is called once per child of a step batch, with
# that child at /cas/args/in = {state, message}.
#
# THE REQUEST IS THE ONE std/actor FORMS. std/actor's start binds exactly these
# three things -- `prepare-request --base:@=<inner> --state:@=<state tree>
# --message:@=<message>` -- so a transition here and a message delivered to a
# live actor in the same (state, message) are one ArgTree and one cache entry.
# Do not bind anything else onto it: an extra arg would split the two.
set -euo pipefail
caos get /cas/args/in
request=$(caos prepare-request --base:@=/cas/args/inner \
  --state:@=/cas/args/in/state --message:@=/cas/args/in/message)
caos run-request-then "$request"
