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

echo "== runnable ArgTrees carry the execution policy ==" >&2
digest=$(printf '0%.0s' {1..64})
request=$("$CAOS_CLI" prepare-request \
  "--base:docker=example.invalid/worker@sha256:$digest" --probe=identity)
policy_oid=$(git ls-tree "$request" execution-policy | while read -r _ _ oid _; do printf '%s' "$oid"; done)
[ -n "$policy_oid" ] || fail "prepared request has no execution-policy entry"
case $(uname -m) in
  aarch64 | arm64) platform=linux/arm64 ;;
  x86_64 | amd64) platform=linux/amd64 ;;
  *) fail "test has no expected Docker platform for $(uname -m)" ;;
esac
expected="container-v1;platform=$platform;network=enabled;entrypoint=/bin/caos;engine-socket=image-opt-in"
[ "$(git cat-file blob "$policy_oid")" = "$expected" ] \
  || fail "prepared request carries the wrong execution policy"

echo "docker-identity: ALL PASS" >&2
