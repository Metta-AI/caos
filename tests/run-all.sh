#!/usr/bin/env bash
# The caos test runner. EVERY test runs as a caos JOB: each tests/<name> is a
# nested-stack job (tests/lib/run-nested.sh) keyed on (runner script, test
# tree, binaries under test, image IDs) — cached, so an unchanged test is an
# instant hit and editing one test's fixtures re-runs only its job. A new
# tests/<name>/cli.sh is picked up automatically; there is no host-driven
# batch (tests/run.sh remains for running one test against the outer stack
# by hand).
#
# Interim (until the flake-build worker, phase D): the binaries come from host
# nix builds and the worker images from the flake's load-* apps; both then
# enter the jobs' cache keys as content (a git tree of binaries, image IDs).
#
# Usage: tests/run-all.sh          Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

# The Rust bin-workers published into the inner std as curry(runner, bin) —
# plus cargo/rustc, which curry onto their own bases (see run-nested.sh).
BIN_WORKERS=(file-count dirs-only deep-deps rgrep cargo rustc bash-tool llm-step)

echo "building caos client + bringing the stack up (once for the suite)..." >&2
nix build .#caos-cli -o result-caos || exit 1
export CAOS_CLI=$PWD/result-caos/bin/caos-cli
export CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
nix run .#caosd -- up >&2 || exit 1
export CAOS_STACK_READY=1

pass=(); fail=()

# ---------------------------------------------------------------------------
# Build the shared inputs once, then fire one job per test.
# ---------------------------------------------------------------------------
echo "== preparing the nested-job inputs (binaries + images) ==" >&2
CLIENT=$PWD/.caos-dev/run-all-client
rm -rf "$CLIENT"; mkdir -p "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "http://localhost:9090"
git -C "$CLIENT" config gc.auto 0
trap 'rm -rf "$CLIENT" 2>/dev/null' EXIT

cp tests/lib/run-nested.sh "$CLIENT/run-nested.sh"

# The binaries under test, built into a scratch dir (out-links must not land
# in the client repo), then copied in as the jobs' `bins` input.
BROOT=$(mktemp -d)
attrs=(server runnerd caos)
for w in "${BIN_WORKERS[@]}"; do attrs+=("worker-$w"); done
for attr in "${attrs[@]}"; do
  nix build ".#$attr" -o "$BROOT/$attr" || exit 1
done
mkdir -p "$CLIENT/bins"
cp -L "$BROOT"/server/bin/server "$BROOT"/runnerd/bin/runnerd \
  "$BROOT"/caos/bin/caos-cli "$CLIENT/bins/"
for w in "${BIN_WORKERS[@]}"; do
  cp -L "$BROOT/worker-$w/bin/worker-$w" "$CLIENT/bins/"
done
# The stub LLM server the chat/llm-step tests script (not a worker — their
# cli.sh runs it inside the job, siblings reach it over the shared netns).
nix build .#llm-stub -o "$BROOT/llm-stub" || exit 1
cp -L "$BROOT/llm-stub/bin/llm-stub" "$CLIENT/bins/"

# The sibling worker images: the flake's own runner + bash images, loaded into
# the outer engine store and referenced by image ID — a content address, so it
# keys the jobs honestly (a `:latest` tag in a cache key would lie). The runner
# ships as a bare delta meant to be stacked on the stock debian base at
# registry-convert time; the production stack does that in the server's convert
# path, and here we reproduce it with a two-line build (the chmod re-asserts
# setuid, which COPY does not reliably carry).
nix run .#load-caos-worker-runner >&2 || exit 1
nix run .#load-caos-worker-bash >&2 || exit 1
ictx=$(mktemp -d)
cat > "$ictx/Containerfile" <<'EOF'
FROM docker.io/library/debian:stable-slim
COPY --from=localhost/caos-worker-runner:latest / /
RUN chmod 4755 /usr/bin/caos
ENTRYPOINT ["/bin/caos","runner"]
EOF
docker build -t localhost/caos-test-runner:latest "$ictx" \
  >/tmp/run-all-imgbuild.log 2>&1 \
  || { tail -20 /tmp/run-all-imgbuild.log >&2
       echo "stacked runner image build failed" >&2; exit 1; }
rm -rf "$ictx"
# The cargo toolchain base image: big and slow to `docker load`, so skip the
# load when the built tarball (its nix store path) is already the one loaded.
nix build .#caos-worker-cargo-base-docker -o "$BROOT/cargo-img" || exit 1
cargo_img=$(readlink -f "$BROOT/cargo-img")
marker=$CAOS_DATA/.cargo-img-loaded
if [ "$(cat "$marker" 2>/dev/null)" != "$cargo_img" ] \
   || ! docker image exists localhost/caos-worker-cargo-base:latest; then
  echo "loading the cargo toolchain image (once per toolchain change)..." >&2
  docker load -i "$cargo_img" >&2 || exit 1
  echo "$cargo_img" > "$marker"
fi

img_id() { docker inspect --format '{{.Id}}' "$1" | sed 's/^sha256://'; }
RUNNER_REF=$(img_id localhost/caos-test-runner:latest) || exit 1
BASH_REF=$(img_id localhost/caos-worker-bash:latest) || exit 1
CARGO_REF=$(img_id localhost/caos-worker-cargo-base:latest) || exit 1

# The worker-common source tree (rustc's curry links generated projects
# against it) and the workspace snapshot (cargo-self dogfoods the tree under
# test; only its job carries it, so other jobs don't re-key on source edits).
cp -R crates/worker-common "$CLIENT/worker-common"
chmod -R u+w "$CLIENT/worker-common"
mkdir -p "$CLIENT/workspace"
git archive HEAD | tar -x -C "$CLIENT/workspace"

TESTS=()
for d in tests/*/; do
  t=$(basename "${d%/}")
  [ -f "$d/cli.sh" ] && TESTS+=("$t")
done
mkdir -p "$CLIENT/cases"
for c in "${TESTS[@]}"; do cp -r "tests/$c" "$CLIENT/cases/$c"; done
( cd "$CLIENT" && git add -A && git -c user.email=t@c -c user.name=c commit -qm setup )

# Jobs run UNSALTED: their isolation is inherent (each stands up its own
# hermetic stack), and the whole point is that an unchanged test is a cache
# hit across runs — a per-run salt would re-key every job every run.
nested() { # <name>
  local extra=()
  [ "$1" = cargo-self ] && extra=(--workspace:@=workspace)
  # The real API key rides as an ordinary arg (it already rides through
  # request args in `caos chat` itself): same key, same cache key — the test
  # re-runs only when the code or the key changes. Without a key the job runs
  # (and caches) the test's own skip path.
  [ "$1" = chat-online ] && [ -n "${ANTHROPIC_API_KEY:-}" ] \
    && extra=(--api_key="$ANTHROPIC_API_KEY")
  echo "=== tests/$1 ===" >&2
  if ( cd "$CLIENT" && CAOS_SALT= "$CAOS_CLI" run /cas/std/testenv "out-$1" \
         -- --script:@=run-nested.sh --test:@="cases/$1" --bins:@=bins \
            --runner_image="$RUNNER_REF" --bash_image="$BASH_REF" \
            --cargo_image="$CARGO_REF" --worker_common:@=worker-common \
            "${extra[@]}" ) \
     && grep -q "RUN-TEST: PASS" "$CLIENT/out-$1"; then
    pass+=("tests/$1"); else fail+=("tests/$1"); fi
}
for c in "${TESTS[@]}"; do nested "$c"; done

echo >&2
echo "==== ${#pass[@]}/$(( ${#pass[@]} + ${#fail[@]} )) passed ====" >&2
for t in "${pass[@]}"; do echo "  PASS $t" >&2; done
for t in "${fail[@]}"; do echo "  FAIL $t" >&2; done
[ "${#fail[@]}" -eq 0 ]
