#!/usr/bin/env bash
# Runs cwd'd into a client repo with $CAOS_CLI set, INSIDE a test stack.
#
# Exercises secret injection with the store carried as ephemeral run context
# (SPEC.md, Secrets): the client reads its own git-ignored `.caos-secrets`,
# assembles each constrained reader against the pinned source tree, and sends
# the partial ArgTree to the server, which subset-matches and injects at
# `/secret/<name>`. Covers constrained grants and denial, copied secret hashes,
# extra unpinned args, path/hash equivalence, multiple readers, configuration
# errors, the output-leak assertion, log masking, and caller propagation.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
object_status() { # <oid>
  curl -sS -o /dev/null -w '%{http_code}' -I "$CAOS_SERVER_URL/object/$1"
}

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
# deploytok's readers require either mytool+environment or bash+role, so it must
# NOT be granted to an otherwise matching token request.
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

# A repo-local tool: a script curried onto the bash image via a .caos-expr, so
# its identity is (image=bash, worker1=run.sh) — what the `mytool` grant
# matches. The image is named by a path WITHIN mytool/ (an expression reaches
# only its own subtree), so the declared bash mount is copied in.
mkdir -p mytool
cp -r DEEP-DEPS/bash mytool/bash
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
curry --base:@=bash --worker1:@=run.sh
EOF

# A child-only reader used to prove that sub-run preserves the whole carried
# store, not merely the values granted to its launching worker.
mkdir -p subtool
cp -r DEEP-DEPS/bash subtool/bash
cat > subtool/run.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/marker
if [ -r /secret/deploytok ] && [ "$(cat /secret/deploytok)" = "DEPLOY-xyz-789" ]; then
  verdict=sub-run-secret-ok
else
  verdict=sub-run-secret-missing
fi
printf '%s:%s\n' "$verdict" "$(cat /cas/args/marker)" > /tmp/sub-run-result
caos put /tmp/sub-run-result /cas/out
EOF
cat > subtool/.caos-expr <<'EOF'
curry --base:@=bash --worker1:@=run.sh
EOF

cat > launch-sub-run.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/request
caos sub-run "$(cat /cas/args/request)" > /tmp/sub-run-admission
caos put /tmp/sub-run-admission /cas/out
EOF

# An EMBEDDER: it binds mytool as a `:@=` arg rather than running it. Since a
# `:@=` target carrying a `.caos-expr` is evaluated (design/caos-expr.md), the
# arg binds mytool's MARKED curry — so the embedder's own arg tree turns over
# with the secret store even though the embedder itself reads no secret. That
# is caller-propagation (design/secrets.md).
#
# mytool is COPIED in, not referenced as `../mytool`: an expression reaches only
# its own subtree. The copy is byte-identical, so it evaluates to the same curry
# the `mytool` reader resolves to, and the grant matches it.
mkdir -p embedder
cp -r DEEP-DEPS/bash embedder/bash
cp -r mytool embedder/mytool
cat > embedder/run.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
echo embedded > /tmp/out/verdict
caos put /tmp/out /cas/out
EOF
cat > embedder/.caos-expr <<'EOF'
curry --base:@=bash --worker1:@=run.sh --pusher:@=mytool
EOF

# A pinned tree argument used to prove that reader-side `:@=` and request-side
# path/hash references resolve to the same oid.
mkdir policy
echo allow-production > policy/destination

echo '.caos-secrets/' > .gitignore
commit "secrets fixtures"
test_run_id="$(date +%s%N)-$$-$RANDOM"
git push -q caos "HEAD:refs/heads/${test_run_id}-secrets-test" \
  || fail "pushing workspace to caos"

# --- the store (git-ignored, per-user, NOT committed) ------------------------
mkdir -p .caos-secrets
# Pin readers' resolution to this committed tree (excludes .caos-secrets itself).
git rev-parse "HEAD^{tree}" > .caos-secrets/.tree
trusted_endpoint=https://trusted.example
bash_image=$("$CAOS_CLI" eval-path DEEP-DEPS/bash) || fail "resolving bash"
bash_image=${bash_image##* }
policy_tree=$(git rev-parse HEAD:policy) || fail "resolving policy tree"
cat > .caos-secrets/token <<EOF
value=SEKRET-abc-123
entropy=7f3a9c2e1b8d4f60a5e7c9d1b3f5a7e9
reader=--base:@=DEEP-DEPS/bash --endpoint=$trusted_endpoint --policy:@=policy
EOF
cat > .caos-secrets/deploytok <<EOF
value=DEPLOY-xyz-789
entropy=aa11bb22cc33dd44ee55ff6677889900
reader=--base:@=mytool --environment=production
reader=--base:@=subtool
reader=--base:@=DEEP-DEPS/bash --role=auditor
reader=--base:hash=$bash_image --policy:hash=$policy_tree --reader-form=hash
EOF
cat > .caos-secrets/propagation <<'EOF'
value=unused-propagation-value
entropy=00112233445566778899aabbccddeeff
reader=--base:@=mytool
EOF

echo "== matching constraints grant; extra unpinned arguments remain free ==" >&2
matching_request=$("$CAOS_CLI" prepare-request \
  --base:@=DEEP-DEPS/bash --worker1:@=check.sh \
  --endpoint="$trusted_endpoint" --policy:@=policy \
  --head=conversation-head --history=conversation-history) \
  || fail "preparing matching request"
"$CAOS_CLI" run got --base:hash="$matching_request" >/dev/null \
  || fail "running matching request"
verdict=$(cat got/verdict)
[ "$verdict" = "token-ok deploy-absent" ] \
  || fail "verdict: $verdict (expected 'token-ok deploy-absent')"
echo "  ok: exact endpoint granted despite unpinned head/history args" >&2

echo "== the same worker at another destination is denied ==" >&2
"$CAOS_CLI" run wrong-endpoint --base:@=DEEP-DEPS/bash --worker1:@=check.sh \
  --endpoint=https://attacker.invalid --policy:@=policy >/dev/null \
  || fail "running mismatched destination"
verdict=$(cat wrong-endpoint/verdict)
[ "$verdict" = "token-bad deploy-absent" ] \
  || fail "wrong-endpoint verdict: $verdict"
echo "  ok: changing only endpoint removes the grant" >&2

echo "== a copied secret-hash cannot authorize a mismatched request ==" >&2
copied_secret_hash=$(git rev-parse "$matching_request:secret-hash") \
  || fail "reading matching secret-hash"
forged_request=$("$CAOS_CLI" prepare-request \
  --base:@=DEEP-DEPS/bash --worker1:@=check.sh \
  --endpoint=https://attacker.invalid --policy:@=policy \
  --secret-hash:hash="$copied_secret_hash") \
  || fail "preparing copied-hash request"
"$CAOS_CLI" run forged --base:hash="$forged_request" >/dev/null \
  || fail "running copied-hash request"
verdict=$(cat forged/verdict)
[ "$verdict" = "token-bad deploy-absent" ] \
  || fail "copied-hash verdict: $verdict"
echo "  ok: copied isolation metadata does not override the reader mismatch" >&2

echo "== path and hash forms resolve to the same base and tree args ==" >&2
"$CAOS_CLI" run hash-forms --base:hash="$bash_image" --worker1:@=check.sh \
  --endpoint="$trusted_endpoint" --policy:hash="$policy_tree" >/dev/null \
  || fail "running hash-form equivalent"
verdict=$(cat hash-forms/verdict)
[ "$verdict" = "token-ok deploy-absent" ] \
  || fail "hash-form verdict: $verdict"
"$CAOS_CLI" run reader-hash-forms --base:@=DEEP-DEPS/bash --worker1:@=check.sh \
  --policy:@=policy --reader-form=hash >/dev/null \
  || fail "running reader hash-form equivalent"
verdict=$(cat reader-hash-forms/verdict)
[ "$verdict" = "token-bad deploy-leaked" ] \
  || fail "reader hash-form verdict: $verdict"
echo "  ok: reader and request path/tree/hash forms share curry oid identity" >&2

echo "== multiple readers keep their own bases and constraints ==" >&2
tool=$("$CAOS_CLI" eval-path mytool) || fail "eval-path mytool failed: $tool"
"$CAOS_CLI" run tool-reader --base:hash="${tool##* }" \
  --environment=production >/dev/null || fail "running mytool reader"
[ "$(cat tool-reader/verdict)" = "deploytok-ok" ] \
  || fail "mytool reader did not receive deploytok"
"$CAOS_CLI" run role-reader --base:@=DEEP-DEPS/bash --worker1:@=check.sh \
  --role=auditor >/dev/null || fail "running role reader"
[ "$(cat role-reader/verdict)" = "token-bad deploy-leaked" ] \
  || fail "role reader did not independently receive deploytok"
echo "  ok: each reader matches only its own base and pinned arguments" >&2

echo "== a worker that leaks a secret into its output is refused ==" >&2
if "$CAOS_CLI" run leaked --base:@=DEEP-DEPS/bash --worker1:@=leak.sh \
    --endpoint="$trusted_endpoint" --policy:@=policy >/dev/null 2>leak.err; then
  fail "run leaking the secret should have failed"
fi
grep -qi "secret" leak.err || fail "leak error should mention a secret: $(cat leak.err)"
if grep -q "SEKRET-abc-123" leak.err; then fail "the error must NOT echo the secret value"; fi
echo "  ok: leaking the value fails the run, without echoing it" >&2

echo "== a secret printed to the log is masked ==" >&2
if "$CAOS_CLI" run shouted --base:@=DEEP-DEPS/bash --worker1:@=shout.sh \
    --endpoint="$trusted_endpoint" --policy:@=policy >/dev/null 2>shout.err; then
  fail "the shouting worker should have failed"
fi
if grep -q "SEKRET-abc-123" shout.err; then
  fail "the secret value must be masked out of the log: $(cat shout.err)"
fi
grep -q "redacted secret" shout.err \
  || fail "expected the redaction marker in the log: $(cat shout.err)"
echo "  ok: the printed secret is masked" >&2

echo "== a grant can name a repo-local tool by path ==" >&2
out=$("$CAOS_CLI" run tgot --base:hash="${tool##* }" --environment=production) \
  || fail "run mytool failed: $out"
verdict=$(cat tgot/verdict)
[ "$verdict" = "deploytok-ok" ] \
  || fail "current-tree grant verdict: $verdict (expected 'deploytok-ok')"
echo "  ok: a reader naming a repo path grants the secret" >&2

echo "== sub-run preserves the server-held secret store ==" >&2
# The launcher is an ordinary bash worker and cannot read deploytok. Its child
# is the independently authorized subtool request; the child succeeds only if
# the server carries the original store through the detached edge.
subtool=$("$CAOS_CLI" eval-path subtool) || fail "eval-path subtool failed: $subtool"
marker="detached-$(date +%s%N)-$$-$RANDOM"
expected="sub-run-secret-ok:$marker"
expected_oid=$(printf '%s\n' "$expected" | git hash-object --stdin)
request=$("$CAOS_CLI" prepare-request --base:hash="${subtool##* }" --marker="$marker") \
  || fail "preparing subtool request failed"
launcher=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=launch-sub-run.sh --request="$request") \
  || fail "currying sub-run launcher failed"
admitted=$("$CAOS_CLI" run --base:hash="$launcher") || fail "sub-run launcher failed"
[ "$admitted" = "request $request" ] \
  || fail "sub-run launcher admitted the wrong request: $admitted"

complete=0
for _ in $(seq 1 150); do
  status=$(object_status "$expected_oid") || fail "server unreachable while waiting for sub-run"
  case "$status" in
    200) complete=1; break ;;
    404) ;;
    *) fail "server returned HTTP $status while waiting for sub-run" ;;
  esac
  sleep 0.1
done
[ "$complete" -eq 1 ] || fail "sub-run never produced its secret-backed result"
"$CAOS_CLI" run sub-run-result --base:hash="$request" >/dev/null \
  || fail "reading completed sub-run failed"
[ "$(cat sub-run-result)" = "$expected" ] \
  || fail "sub-run result was wrong: $(cat sub-run-result)"
echo "  ok: a child received its reader-matched secret through server context" >&2

echo "== eval-path folds secret-hash, so a worker's callers become per-user ==" >&2
# mytool matches the deploytok reader, so eval-path marks its returned arg tree.
# With the secret removed it matches nothing, so the result differs — which is
# exactly what makes anything embedding mytool per-user.
withsecret=$("$CAOS_CLI" eval-path mytool) || fail "eval-path mytool failed"
emb_with=$("$CAOS_CLI" eval-path embedder) || fail "eval-path embedder failed"
rm .caos-secrets/propagation
without=$("$CAOS_CLI" eval-path mytool) || fail "eval-path mytool (no secret) failed"
emb_without=$("$CAOS_CLI" eval-path embedder) || fail "eval-path embedder (no secret) failed"
[ "$withsecret" != "$without" ] \
  || fail "eval-path mytool should depend on the secret store (got '$withsecret' both times)"
echo "  ok: mytool's eval result differs with vs without its granted secret" >&2

echo "== caller-propagation: embedding a granted worker isolates the EMBEDDER ==" >&2
# The embedder reads no secret; it only binds mytool via `--pusher:@=mytool`.
# Its result must still differ, or a caller of a secret-granted worker would
# share one cache entry across users (design/secrets.md, caller-propagation).
[ "$emb_with" != "$emb_without" ] \
  || fail "eval-path embedder should depend on the store (got '$emb_with' both times)"
echo "  ok: the embedder is per-user via its :@= worker, reading no secret itself" >&2

echo "== malformed and unsafe reader configurations fail clearly ==" >&2
expect_reader_error() { # <label> <reader> <message-fragment>
  local label=$1 reader=$2 expected=$3
  printf '%s\n' \
    'value=invalid-reader-fixture' \
    'entropy=ffeeddccbbaa99887766554433221100' \
    "reader=$reader" > ".caos-secrets/invalid-$label"
  if "$CAOS_CLI" eval-path DEEP-DEPS/bash >/dev/null 2>reader.err; then
    fail "$label reader unexpectedly succeeded"
  fi
  grep -qF -- "$expected" reader.err \
    || fail "$label reader error did not mention $expected: $(cat reader.err)"
  rm ".caos-secrets/invalid-$label"
}
expect_reader_error missing-base '--scope=x' 'needs a --base'
expect_reader_error duplicate \
  '--base:@=DEEP-DEPS/bash --scope=x --scope=y' 'provided more than once'
expect_reader_error rebound \
  '--base:@=mytool --worker1:@=check.sh' 'already bound'
expect_reader_error malformed \
  '--base:@=DEEP-DEPS/bash not-an-argument' 'argument must look like'
expect_reader_error untyped '--base=DEEP-DEPS/bash' '--base needs a type'
blob_base=$(git rev-parse HEAD:check.sh) || fail "resolving blob base fixture"
expect_reader_error unsuitable "--base:hash=$blob_base" 'not an image tree'
expect_reader_error old-shorthand 'DEEP-DEPS/bash' 'argument must look like'
echo "  ok: missing, duplicate, rebound, malformed, untyped, unsuitable, and old forms fail" >&2

echo "== caos-cli secrets fills missing entropy and --check gates weak ones ==" >&2
mkdir -p /tmp/sectool/.caos-secrets
cd /tmp/sectool
printf 'value=abc\nreader=--base:@=DEEP-DEPS/bash\n' > .caos-secrets/needs-entropy
"$CAOS_CLI" secrets || fail "secrets fill failed"
grep -q '^entropy=' .caos-secrets/needs-entropy || fail "entropy was not filled in"
# A present-but-weak entropy is never overwritten, and --check must gate on it.
printf 'value=abc\nentropy=short\nreader=--base:@=DEEP-DEPS/bash\n' > .caos-secrets/weak
if "$CAOS_CLI" secrets --check 2>/dev/null; then
  fail "--check should exit non-zero on weak entropy"
fi
echo "  ok: entropy autofilled; --check gates weak entropy" >&2

echo "secrets: ALL PASS" >&2
