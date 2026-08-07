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
out=$("$CAOS_CLI" run /cas/std/bash got -- --worker1:@=check.sh) || fail "run failed: $out"
verdict=$(cat got/verdict)
[ "$verdict" = "token-ok locked-absent" ] \
  || fail "verdict: $verdict (expected 'token-ok locked-absent')"
echo "  ok: /secret/token granted with the right value, /secret/locked denied" >&2

echo "== a worker that leaks a secret into its output is refused ==" >&2
cat > leak.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
# Copy the raw secret into the output tree — the output-leak assertion must
# refuse to publish this, failing the run.
cat /secret/token > /tmp/out/leaked
caos put /tmp/out /cas/out
EOF
commit "leak fixture"
if "$CAOS_CLI" run /cas/std/bash leaked -- --worker1:@=leak.sh >/dev/null 2>leak.err; then
  fail "run leaking the secret should have failed"
fi
grep -qi "secret" leak.err || fail "leak error should mention a secret: $(cat leak.err)"
if grep -q "SEKRET-abc-123" leak.err; then fail "the error must NOT echo the secret value"; fi
echo "  ok: leaking the secret value into output fails the run, without echoing it" >&2

echo "== a secret printed to the log is masked ==" >&2
cat > shout.sh <<'EOF'
#!/bin/bash
# Print the secret to stderr, then fail so the worker log surfaces to the
# client. The value must be masked out of that log.
echo "debug: token=SEKRET-abc-123 (oops)" >&2
exit 1
EOF
commit "mask fixture"
if "$CAOS_CLI" run /cas/std/bash shouted -- --worker1:@=shout.sh >/dev/null 2>shout.err; then
  fail "the shouting worker should have failed"
fi
if grep -q "SEKRET-abc-123" shout.err; then
  fail "the secret value must be masked out of the surfaced log: $(cat shout.err)"
fi
grep -q "redacted secret" shout.err \
  || fail "expected the redaction marker in the log: $(cat shout.err)"
echo "  ok: the printed secret is replaced by the redaction marker" >&2

echo "== a grant can name a tool in the current tree ==" >&2
# A repo-local tool: a bash script curried onto /std/bash via a .caos-expr, so
# its identity is (image=bash, worker1=run.sh) — what the grant must match.
mkdir -p mytool
cat > mytool/run.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
if [ -r /secret/deploytok ] && [ "$(cat /secret/deploytok)" = "DEPLOY-xyz-789" ]; then
  echo "deploytok-ok" > /tmp/out/verdict
else
  echo "deploytok-missing" > /tmp/out/verdict
fi
caos put /tmp/out /cas/out
EOF
cat > mytool/.caos-expr <<'EOF'
curry /std/bash -- --worker1:@=run.sh
EOF
# git add -A here makes the working tree == HEAD^{tree}, so eval-path (which
# ingests the working tree) publishes exactly the tree we point the store at.
commit "current-tree tool"

# Point the store's "current tree" at this commit, and grant by repo path.
# Push the commit so its tree closure is on the server for the grant to resolve.
git push -q caos HEAD:refs/heads/secrets-test || fail "pushing workspace to caos"
git rev-parse "HEAD^{tree}" > "$SECRETS_DIR/.tree"
cat > "$SECRETS_DIR/deploytok" <<'EOF'
value=DEPLOY-xyz-789
reader=mytool
EOF
chmod -R a+rX "$SECRETS_DIR"

tool=$("$CAOS_CLI" eval-path mytool) || fail "eval-path mytool failed: $tool"
out=$("$CAOS_CLI" run "${tool##* }" tgot --) || fail "run mytool failed: $out"
verdict=$(cat tgot/verdict)
[ "$verdict" = "deploytok-ok" ] \
  || fail "current-tree grant verdict: $verdict (expected 'deploytok-ok')"
echo "  ok: a reader naming a repo path (via its .caos-expr) grants the secret" >&2

echo "secrets: ALL PASS" >&2
