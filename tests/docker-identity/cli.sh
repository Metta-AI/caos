#!/usr/bin/env bash
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== tag-based worker refs fail before dispatch ==" >&2
if "$CAOS_CLI" run --base:docker=example.invalid/worker:latest 2>tag.err; then
  fail "server accepted a mutable top-level Docker ref"
fi
grep -q "is mutable" tag.err || fail "top-level rejection did not explain mutability"
grep -q "@sha256" tag.err || fail "top-level rejection did not prescribe a digest"

echo "== tag-based bases in git images fail before conversion ==" >&2
mkdir tag-image
printf '%s' 'docker://example.invalid/base:latest' >tag-image/base
printf '%s' '{}' >tag-image/config.json
git add tag-image
git -c user.email=test@caos -c user.name=caos commit -qm "tag-based git image"
if "$CAOS_CLI" run --base:@=tag-image 2>base.err; then
  fail "server accepted a mutable Docker base embedded in a git image"
fi
grep -q "git image base" base.err || fail "embedded-base rejection lost its context"
grep -q "is mutable" base.err || fail "embedded-base rejection did not explain mutability"

echo "docker-identity: ALL PASS" >&2
