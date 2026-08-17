#!/usr/bin/env bash
set -euo pipefail

# Returning our own complete ArgTree hash lets the caller prove that an exact
# request continuation executes precisely R rather than reconstructing a near
# equivalent request around its image.
caos hash /cas/args > /tmp/request
caos put /tmp/request /cas/out
