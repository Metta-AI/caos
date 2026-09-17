#!/usr/bin/env bash
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }

# No GitHub account or external mutation: version exercises the real image,
# secret grant and invocation record.
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' 'value=github-fixture-token' 'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/github' > .caos-secrets/github-token
id=$(printf '%s' "$(date +%s%N)-$$-$RANDOM" | sha256sum)
id=${id%% *}
"$CAOS_CLI" run version --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--version"]' --invocation="$id"
jq -e '.status == "complete" and .exit == 0 and (.stdout | contains("gh version"))' version >/dev/null
"$CAOS_CLI" run repeated --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--version"]' --invocation="$id"
cmp version repeated
if "$CAOS_CLI" run changed --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--help"]' --invocation="$id" 2>changed.err; then
  fail "an invocation accepted changed arguments"
fi
grep -q "different request" changed.err || fail "unclear invocation error"

echo "github: ALL PASS"
