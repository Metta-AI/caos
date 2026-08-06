#!/usr/bin/env bash
# Runs cwd'd into a client repo with $CAOS_CLI set, INSIDE a test stack — the
# suite's per-test job (tests/lib/run-test.sh).
#
# Exercises secret injection (design/secrets.md): a secret in the server's
# git-ignored `.caos-secrets` store is dropped at `/secret/<name>` in a worker
# whose ArgTree is a SUPERSET of one of the secret's readers, and nowhere else.
# The value never rides in the ArgTree, so it is not in the cache key.
#
# We register two secrets and run a std/bash worker:
#   - `token`   reader `std/bash`            -> matches (superset on image): granted
#   - `locked`  reader `std/bash --marker=…` -> pins an arg this job lacks: denied
# The worker reports a LEAK-FREE verdict (never the value itself), so this test
# stays valid once the output-scrub assertion lands.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# The inner stack keeps its state at CAOS_STACK_STATE=/tmp/stack
# (test-stack/worker), and stack/serve points the server's CAOS_SECRETS_DIR at
# <state>/secrets. The server reads it fresh per dispatch, so registering here —
# after bring-up — is enough. Same container as the inner server, so it can
# read what we write.
SECRETS_DIR=/tmp/stack/secrets
mkdir -p "$SECRETS_DIR"

# Granted: any std/bash worker's ArgTree is a superset of the bare-image reader.
cat > "$SECRETS_DIR/token" <<'EOF'
# a test token
value=SEKRET-abc-123
reader=std/bash
EOF

# Denied: the reader pins --marker=nope, which this run does not pass.
cat > "$SECRETS_DIR/locked" <<'EOF'
value=NOPE-do-not-leak
reader=std/bash -- --marker=nope
EOF
chmod -R a+rX "$SECRETS_DIR"

# The worker: read the granted secret, confirm the denied one is absent, and
# emit a verdict that never contains a secret value.
cat > check.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
verdict=""
if [ -r /secret/token ]; then
  if [ "$(cat /secret/token)" = "SEKRET-abc-123" ]; then
    verdict="token-ok"
  else
    verdict="token-wrong"
  fi
else
  verdict="token-missing"
fi
if [ -e /secret/locked ]; then
  verdict="$verdict locked-leaked"
else
  verdict="$verdict locked-absent"
fi
echo "$verdict" > /tmp/out/verdict
caos put /tmp/out /cas/out
EOF
commit "secrets fixtures"

echo "== a granted secret is injected; a non-matching one is not ==" >&2
out=$("$CAOS_CLI" run /cas/std/bash -- --worker1:@=check.sh) || fail "run failed: $out"
hash=${out##* }
"$CAOS_CLI" get "$hash" got || fail "get $hash"
verdict=$(cat got/verdict)
[ "$verdict" = "token-ok locked-absent" ] \
  || fail "verdict: $verdict (expected 'token-ok locked-absent')"
echo "  ok: /secret/token granted with the right value, /secret/locked denied" >&2

echo "secrets: ALL PASS" >&2
