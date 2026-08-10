#!/usr/bin/env bash
# Runs cwd'd into a client repo with $CAOS_CLI set, INSIDE a test stack.
#
# Exercises secret injection with the store carried as ephemeral run context
# (design/secrets.md): the client reads its own git-ignored `.caos-secrets`,
# resolves each reader with eval-path, and sends the result to the server,
# which subset-matches and injects at `/secret/<name>`. Covers: a granted vs a
# denied secret, the output-leak assertion, log masking, and a grant naming a
# repo-local tool by path.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# --- fixtures (committed) ----------------------------------------------------
# A worker that reads the granted secret and reports a leak-free verdict.
cat > check.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
if [ -r /secret/token ] && [ "$(cat /secret/token)" = "SEKRET-abc-123" ]; then
  v="token-ok"
else
  v="token-bad"
fi
# deploytok's reader (mytool) is a DIFFERENT expression, so it must NOT be
# granted to this plain bash worker.
[ -e /secret/deploytok ] && v="$v deploy-leaked" || v="$v deploy-absent"
echo "$v" > /tmp/out/verdict
caos put /tmp/out /cas/out
EOF

# A worker that copies the secret into its output — the leak assertion must
# refuse to publish it.
cat > leak.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
cat /secret/token > /tmp/out/leaked
caos put /tmp/out /cas/out
EOF

# A worker that prints the secret then fails — it must be masked from the log.
cat > shout.sh <<'EOF'
#!/bin/bash
echo "debug: token=SEKRET-abc-123 (oops)" >&2
exit 1
EOF

# A repo-local tool: a script curried onto /std/bash via a .caos-expr, so its
# identity is (image=bash, worker1=run.sh) — what the `mytool` grant matches.
mkdir -p mytool
cat > mytool/run.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
if [ -r /secret/deploytok ] && [ "$(cat /secret/deploytok)" = "DEPLOY-xyz-789" ]; then
  echo deploytok-ok > /tmp/out/verdict
else
  echo deploytok-missing > /tmp/out/verdict
fi
caos put /tmp/out /cas/out
EOF
cat > mytool/.caos-expr <<'EOF'
curry /std/bash -- --worker1:@=run.sh
EOF

echo '.caos-secrets/' > .gitignore
commit "secrets fixtures"
# Publish the commit so the pinned tree (below) is on the server for the
# `mytool` reader to resolve against.
git push -q caos HEAD:refs/heads/secrets-test || fail "pushing workspace to caos"

# --- the store (git-ignored, per-user, NOT committed) ------------------------
mkdir -p .caos-secrets
# Pin readers' resolution to this committed tree (excludes .caos-secrets itself).
git rev-parse "HEAD^{tree}" > .caos-secrets/.tree
cat > .caos-secrets/token <<'EOF'
value=SEKRET-abc-123
entropy=7f3a9c2e1b8d4f60a5e7c9d1b3f5a7e9
reader=std/bash
EOF
cat > .caos-secrets/deploytok <<'EOF'
value=DEPLOY-xyz-789
entropy=aa11bb22cc33dd44ee55ff6677889900
reader=mytool
EOF

echo "== a granted secret is injected; a non-matching one is not ==" >&2
out=$("$CAOS_CLI" run /cas/std/bash got -- --worker1:@=check.sh) || fail "run failed: $out"
verdict=$(cat got/verdict)
[ "$verdict" = "token-ok deploy-absent" ] \
  || fail "verdict: $verdict (expected 'token-ok deploy-absent')"
echo "  ok: /secret/token granted, the differently-scoped /secret/deploytok denied" >&2

echo "== a worker that leaks a secret into its output is refused ==" >&2
if "$CAOS_CLI" run /cas/std/bash leaked -- --worker1:@=leak.sh >/dev/null 2>leak.err; then
  fail "run leaking the secret should have failed"
fi
grep -qi "secret" leak.err || fail "leak error should mention a secret: $(cat leak.err)"
if grep -q "SEKRET-abc-123" leak.err; then fail "the error must NOT echo the secret value"; fi
echo "  ok: leaking the value fails the run, without echoing it" >&2

echo "== a secret printed to the log is masked ==" >&2
if "$CAOS_CLI" run /cas/std/bash shouted -- --worker1:@=shout.sh >/dev/null 2>shout.err; then
  fail "the shouting worker should have failed"
fi
if grep -q "SEKRET-abc-123" shout.err; then
  fail "the secret value must be masked out of the log: $(cat shout.err)"
fi
grep -q "redacted secret" shout.err \
  || fail "expected the redaction marker in the log: $(cat shout.err)"
echo "  ok: the printed secret is masked" >&2

echo "== a grant can name a repo-local tool by path ==" >&2
tool=$("$CAOS_CLI" eval-path mytool) || fail "eval-path mytool failed: $tool"
out=$("$CAOS_CLI" run "${tool##* }" tgot --) || fail "run mytool failed: $out"
verdict=$(cat tgot/verdict)
[ "$verdict" = "deploytok-ok" ] \
  || fail "current-tree grant verdict: $verdict (expected 'deploytok-ok')"
echo "  ok: a reader naming a repo path grants the secret" >&2

echo "== caos-cli secrets fills missing entropy and --check gates weak ones ==" >&2
mkdir -p /tmp/sectool/.caos-secrets
cd /tmp/sectool
printf 'value=abc\nreader=std/bash\n' > .caos-secrets/needs-entropy
"$CAOS_CLI" secrets || fail "secrets fill failed"
grep -q '^entropy=' .caos-secrets/needs-entropy || fail "entropy was not filled in"
# A present-but-weak entropy is never overwritten, and --check must gate on it.
printf 'value=abc\nentropy=short\nreader=std/bash\n' > .caos-secrets/weak
if "$CAOS_CLI" secrets --check 2>/dev/null; then
  fail "--check should exit non-zero on weak entropy"
fi
echo "  ok: entropy autofilled; --check gates weak entropy" >&2

echo "secrets: ALL PASS" >&2
