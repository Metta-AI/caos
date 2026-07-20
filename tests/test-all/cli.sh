#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# The suite as PER-TEST caos jobs (design/cargo-workers.md): for each foldable
# test, fire one testenv job that runs that test's REAL cli.sh inside a nested
# stack — keyed on (run-test.sh, that test's tree, the built binaries, backend +
# images). A second pass is all cache hits, and editing one test's fixtures
# re-runs only its job. Two backends per test:
#   process  curry-able Rust bin-workers (fast chroot slots)   — file-count, …
#   socket   image-based workers via the granted engine socket — the bash SCRIPT
#            worker tests (symlinks, untracked, run-then)
# Still host-driven (not folded): tests whose cli.sh needs `nix` (bash-tool),
# the heavy toolchain-image tests (cargo*/rust-worker/commit), the network/stub
# tests (chat*/llm-step), and the meta nested-stack tests themselves.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# name:backend
PROCESS_CASES=(file-count dirs-only deep-deps rgrep)
SOCKET_CASES=(symlinks untracked run-then)

echo "== building the inner-stack binaries ==" >&2
nix build "$CAOS_PROJECT#server" -o srv
nix build "$CAOS_PROJECT#runnerd" -o rnd
nix build "$CAOS_PROJECT#caos" -o cs
nix build "$CAOS_PROJECT#worker-runner" -o wr
nix build "$CAOS_PROJECT#worker-file-count" -o wfc
nix build "$CAOS_PROJECT#worker-dirs-only" -o wdo
nix build "$CAOS_PROJECT#worker-deep-deps" -o wdd
nix build "$CAOS_PROJECT#worker-rgrep" -o wrg
mkdir -p bins
cp -L srv/bin/server rnd/bin/runnerd cs/bin/caos cs/bin/caos-cli wr/bin/worker-runner \
  wfc/bin/worker-file-count wdo/bin/worker-dirs-only wdd/bin/worker-deep-deps \
  wrg/bin/worker-rgrep bins/

echo "== building the bash SCRIPT worker image (for the socket cases) ==" >&2
# A self-contained image the OUTER engine store can run: static setuid caos +
# the generic bash-script /worker (fetch /cas/args/script, run it). Static bins
# on debian (bash + coreutils already there). Stable tag so job inputs are
# stable; the inner server passes docker://<tag> straight through.
BASH_IMAGE=localhost/caos-bash-worker:latest
ictx=$(mktemp -d)
cp -L cs/bin/caos "$ictx/caos"
cat > "$ictx/worker" <<'WORKER'
#!/bin/bash
set -euo pipefail
caos get /cas/args/script
bash /cas/args/script
if [ ! -e /cas/out ]; then
  : > /tmp/caos-empty-out
  caos put /tmp/caos-empty-out /cas/out
fi
WORKER
cat > "$ictx/Containerfile" <<EOF
FROM debian:stable-slim
COPY caos /bin/caos
COPY worker /worker
RUN chmod 4755 /bin/caos && chmod 0755 /worker
EOF
docker build -t "$BASH_IMAGE" "$ictx" >/tmp/testall-imgbuild.log 2>&1 \
  || { tail -20 /tmp/testall-imgbuild.log >&2; fail "bash worker image build failed"; }
rm -rf "$ictx"

# Bring each case's real test dir into the client repo so caos-cli can ingest it
# (it hashes git-tracked paths only). They live beside us via CAOS_PROJECT.
mkdir -p cases
for c in "${PROCESS_CASES[@]}" "${SOCKET_CASES[@]}"; do
  cp -r "$CAOS_PROJECT/tests/$c" "cases/$c"
done
commit "binaries + cases"

run_case() { # <name> <backend> <result-dir>
  local extra=()
  [ "$2" = socket ] && extra=(--bash_image="$BASH_IMAGE")
  "$CAOS_CLI" run /cas/std/testenv "$3" \
    -- --script:@=test/run-test.sh --test:@="cases/$1" --bins:@=bins \
       --backend="$2" "${extra[@]}"
}

run_all() { # <result-prefix> -> run every case, echo nothing (caller checks)
  for c in "${PROCESS_CASES[@]}"; do run_case "$c" process "$1-$c"; done
  for c in "${SOCKET_CASES[@]}"; do run_case "$c" socket "$1-$c"; done
}

echo "== each case as its own caos job ==" >&2
t0=$(ms)
run_all r
for c in "${PROCESS_CASES[@]}" "${SOCKET_CASES[@]}"; do
  grep -q "RUN-TEST: PASS" "r-$c" || fail "$c did not pass"
  echo "  ok: $c" >&2
done
t1=$(ms)
echo "  all cases passed ($((t1 - t0))ms cold)" >&2

echo "== second pass: every case is a cache hit ==" >&2
t2=$(ms)
run_all r2
for c in "${PROCESS_CASES[@]}" "${SOCKET_CASES[@]}"; do
  cmp -s "r-$c" "r2-$c" || fail "$c cached verdict differs"
done
t3=$(ms)
echo "  ok: cached ($((t3 - t2))ms vs $((t1 - t0))ms cold)" >&2
# The cached pass must be dramatically cheaper — proof the jobs memoized.
[ "$((t3 - t2))" -lt "$(( (t1 - t0) / 4 ))" ] \
  || fail "cached pass ($((t3 - t2))ms) not much cheaper than cold ($((t1 - t0))ms)"
