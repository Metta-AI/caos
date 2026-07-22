#!/usr/bin/env bash
# The caos test runner: bring the stack up, fire THE SUITE JOB, print its
# report. The suite itself is a caos worker (tests/lib/suite.sh): it run-thens
# the workspace build (std/cargo — the old, known-good caos building the
# edited tree), fans out one job per tests/<name>/cli.sh (map-then, so
# parallelism is slot-bounded by the runner pool), and summarizes. Every
# level is cached: an unchanged test never re-runs, and an unchanged EVERYTHING
# is one suite-level cache hit. An agent inside caos fires the identical job —
# this script is just the host's front door.
#
# Jobs run unsalted by default so caching works across runs; export CAOS_SALT
# to force a re-run (e.g. to retry a flaky failure — failed verdicts are
# values and cache like results).
#
# Host nix remains only for the CLI, the stack itself, and the worker images
# (the flake-build worker, phase D, folds those too). tests/run.sh remains for
# running one test against the outer stack by hand.
#
# Usage: tests/run-all.sh [name...]   Exits non-zero if any test fails.
# With names, the suite job runs just those tests (a filtered suite caches
# separately, but its per-test jobs share their cache with full runs — so
# `run-all.sh symlinks` after a full run is all hits, and vice versa).
set -uo pipefail
cd "$(dirname "$0")/.."

ONLY=("$@")
for t in "${ONLY[@]}"; do
  [ -f "tests/$t/cli.sh" ] || { echo "no such test: tests/$t/cli.sh" >&2; exit 2; }
done

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
# The suite job.
# ---------------------------------------------------------------------------
extra=()
# The real API key rides as an ordinary arg; stage 2 places it in
# chat-online's map child alone, so only that test re-keys when it rotates.
[ -n "${ANTHROPIC_API_KEY:-}" ] && extra+=(--api_key="$ANTHROPIC_API_KEY")
[ "${#ONLY[@]}" -gt 0 ] && extra+=(--only="${ONLY[*]}")

echo "== firing the suite job ==" >&2
CAOS_SALT="${CAOS_SALT:-}" "$CAOS_CLI" run /cas/std/bash "$OUT/suite" -- \
  --script:@=tests/lib/suite.sh \
  --stage2:@=tests/lib/suite-stage2.sh \
  --summarize:@=tests/lib/suite-summarize.sh \
  --run_nested:@=tests/lib/run-nested.sh \
  --workspace:@=. \
  --target="$(uname -m)-unknown-linux-musl" \
  --runner_image="$RUNNER_REF" --bash_image="$BASH_REF" \
  --cargo_image="$CARGO_REF" "${extra[@]}" >/dev/null \
  || { echo "suite job failed" >&2; exit 1; }

echo >&2
cat "$OUT/suite/report" >&2
# A failing test's verdict carries its cli.sh output tail — show it.
for v in "$OUT"/suite/verdicts/*; do
  grep -q "^RUN-TEST: PASS" "$v" && continue
  { echo; echo "---- tests/$(basename "$v") ----"; sed 1d "$v"; } >&2
done
grep -q "^SUITE OK" "$OUT/suite/report"
