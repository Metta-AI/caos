#!/usr/bin/env bash
# The caos test runner. EVERY test runs as a caos JOB: each tests/<name> is a
# nested-stack job (tests/lib/run-nested.sh) keyed on (runner script, test
# tree, binaries under test, image IDs) — cached, so an unchanged test is an
# instant hit and editing one test's fixtures re-runs only its job. A new
# tests/<name>/cli.sh is picked up automatically; there is no host-driven
# batch (tests/run.sh remains for running one test against the outer stack
# by hand).
#
# The binaries under test are built BY caos (phase B): one std/cargo build
# job on the outer stack whose result is just the bin tree, threaded to every
# test job as `--bins:tree=<hash>`. The suite runs FROM this repo — inputs
# are ingested straight from the tracked worktree (dirty tracked edits
# included: the tests see the tree as you edited it), and job outputs land
# under .caos-dev. Host nix remains only for the CLI, the stack itself, and
# the worker images (the flake-build worker, phase D, folds those too).
#
# Usage: tests/run-all.sh          Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

echo "building caos client + bringing the stack up (once for the suite)..." >&2
nix build .#caos-cli -o result-caos || exit 1
export CAOS_CLI=$PWD/result-caos/bin/caos-cli
export CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
nix run .#caosd -- up >&2 || exit 1
export CAOS_STACK_READY=1

# The caos remote is how the CLI finds the server; require it rather than
# mutating this repo behind the user's back.
git remote get-url caos >/dev/null 2>&1 || {
  echo "tests/run-all.sh: this repo needs a 'caos' remote naming the local server:" >&2
  echo "  git remote add caos http://localhost:9090" >&2
  exit 1
}
OUT=$PWD/.caos-dev/run-all
rm -rf "$OUT" && mkdir -p "$OUT"

pass=(); fail=()

# ---------------------------------------------------------------------------
# The sibling worker images (phase-D fodder: these become caos jobs).
# ---------------------------------------------------------------------------
# The flake's own runner + bash images, loaded into the outer engine store and
# referenced by image ID — a content address, so it keys the jobs honestly (a
# `:latest` tag in a cache key would lie). The runner ships as a bare delta
# meant to be stacked on the stock debian base at registry-convert time; the
# production stack does that in the server's convert path, and here we
# reproduce it with a two-line build (the chmod re-asserts setuid, which COPY
# does not reliably carry).
nix run .#load-caos-worker-runner >&2 || exit 1
nix run .#load-caos-worker-bash >&2 || exit 1
stacked_runner_build_ctx=$(mktemp -d)
cat > "$stacked_runner_build_ctx/Containerfile" <<'EOF'
FROM docker.io/library/debian:stable-slim
COPY --from=localhost/caos-worker-runner:latest / /
RUN chmod 4755 /usr/bin/caos
ENTRYPOINT ["/bin/caos","runner"]
EOF
docker build -t localhost/caos-test-runner:latest "$stacked_runner_build_ctx" \
  >/tmp/run-all-imgbuild.log 2>&1 \
  || { tail -20 /tmp/run-all-imgbuild.log >&2
       echo "stacked runner image build failed" >&2; exit 1; }
rm -rf "$stacked_runner_build_ctx"
# The cargo toolchain base image: big and slow to `docker load`, so skip the
# load when the built tarball (its nix store path) is already the one loaded.
nix build .#caos-worker-cargo-base-docker -o /tmp/run-all-cargo-img || exit 1
cargo_img=$(readlink -f /tmp/run-all-cargo-img)
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

# ---------------------------------------------------------------------------
# The binaries under test, built by caos: a bash job that run-thens std/cargo
# over the workspace and strips the result to the bin tree (see
# tests/lib/build-bins.sh for why the strip matters). Unsalted: same tree,
# same binaries, cache hit.
# ---------------------------------------------------------------------------
echo "== building the workspace binaries in caos (std/cargo) ==" >&2
bins_line=$(CAOS_SALT= "$CAOS_CLI" run /cas/std/bash "$OUT/bins" -- \
  --script:@=tests/lib/build-bins.sh --strip:@=tests/lib/strip-bins.sh \
  --target="$(uname -m)-unknown-linux-musl" --workspace:@=.) \
  || { echo "workspace build failed" >&2; exit 1; }
BINS_HASH=${bins_line#tree }
echo "  bins: $BINS_HASH" >&2

# ---------------------------------------------------------------------------
# One job per test. Unsalted: isolation is inherent (each stands up its own
# hermetic stack), and an unchanged test must be a cache hit across runs.
# ---------------------------------------------------------------------------
nested() { # <name>
  local extra=()
  [ "$1" = cargo-self ] && extra+=(--workspace:@=.)
  # The real API key rides as an ordinary arg (it already rides through
  # request args in `caos chat` itself): same key, same cache key — the test
  # re-runs only when the code or the key changes. Without a key the job runs
  # (and caches) the test's own skip path.
  [ "$1" = chat-online ] && [ -n "${ANTHROPIC_API_KEY:-}" ] \
    && extra+=(--api_key="$ANTHROPIC_API_KEY")
  echo "=== tests/$1 ===" >&2
  if CAOS_SALT= "$CAOS_CLI" run /cas/std/testenv "$OUT/out-$1" \
       -- --script:@=tests/lib/run-nested.sh --test:@="tests/$1" \
          --bins:tree="$BINS_HASH" \
          --runner_image="$RUNNER_REF" --bash_image="$BASH_REF" \
          --cargo_image="$CARGO_REF" --worker_common:@=crates/worker-common \
          "${extra[@]}" >/dev/null \
     && grep -q "RUN-TEST: PASS" "$OUT/out-$1"; then
    pass+=("tests/$1"); else fail+=("tests/$1"); fi
}
for d in tests/*/; do
  t=$(basename "${d%/}")
  [ -f "$d/cli.sh" ] && nested "$t"
done

echo >&2
echo "==== ${#pass[@]}/$(( ${#pass[@]} + ${#fail[@]} )) passed ====" >&2
for t in "${pass[@]}"; do echo "  PASS $t" >&2; done
for t in "${fail[@]}"; do echo "  FAIL $t" >&2; done
[ "${#fail[@]}" -eq 0 ]
