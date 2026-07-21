#!/usr/bin/env bash
# The caos test runner. Foldable tests run as per-test caos JOBS: each is a
# nested-stack job (tests/lib/run-nested.sh) keyed on (runner script, test
# tree, binaries under test, image IDs) — cached, so an unchanged test is an
# instant hit and editing one test's fixtures re-runs only its job. The rest
# still run host-driven, until they're folded too (design/cargo-workers.md:
# the toolchain-image tests need their images passed by ID; bash-tool /
# chat-offline / llm-step need their helper builds moved to nested runs
# against the outer std instead of host nix).
#
# Interim (until the flake-build worker, phase D): the binaries come from host
# nix builds and the worker images from the flake's load-* apps; both then
# enter the jobs' cache keys as content (a git tree of binaries, image IDs).
#
# Usage: tests/run-all.sh          Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

# Tests that run as nested caos jobs today. Their inner std: the real bash
# worker image + curry(runner image, bin) for the Rust bin-workers.
FOLD=(file-count dirs-only deep-deps rgrep symlinks untracked run-then)
BIN_WORKERS=(file-count dirs-only deep-deps rgrep)

echo "building caos client + bringing the stack up (once for the suite)..." >&2
nix build .#caos-cli -o result-caos || exit 1
export CAOS_CLI=$PWD/result-caos/bin/caos-cli
export CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
nix run .#caosd -- up >&2 || exit 1
export CAOS_STACK_READY=1
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

pass=(); fail=()

# ---------------------------------------------------------------------------
# Nested batch: build the shared inputs once, then fire one job per test.
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
img_id() { docker inspect --format '{{.Id}}' "$1" | sed 's/^sha256://'; }
RUNNER_REF=$(img_id localhost/caos-test-runner:latest) || exit 1
BASH_REF=$(img_id localhost/caos-worker-bash:latest) || exit 1

mkdir -p "$CLIENT/cases"
for c in "${FOLD[@]}"; do cp -r "tests/$c" "$CLIENT/cases/$c"; done
( cd "$CLIENT" && git add -A && git -c user.email=t@c -c user.name=c commit -qm setup )

nested() { # <name>
  echo "=== tests/$1 (nested) ===" >&2
  if ( cd "$CLIENT" && "$CAOS_CLI" run /cas/std/testenv "out-$1" \
         -- --script:@=run-nested.sh --test:@="cases/$1" --bins:@=bins \
            --runner_image="$RUNNER_REF" --bash_image="$BASH_REF" ) \
     && grep -q "RUN-TEST: PASS" "$CLIENT/out-$1"; then
    pass+=("tests/$1"); else fail+=("tests/$1"); fi
}
for c in "${FOLD[@]}"; do nested "$c"; done

# ---------------------------------------------------------------------------
# Host-driven batch: everything else with a cli.sh (pending fold).
# ---------------------------------------------------------------------------
folded=" ${FOLD[*]} "
for d in tests/*/; do
  t=$(basename "${d%/}")
  [ -f "$d/cli.sh" ] || continue
  case "$folded" in *" $t "*) continue;; esac
  echo "=== tests/$t (host) ===" >&2
  if tests/run.sh "tests/$t"; then pass+=("tests/$t"); else fail+=("tests/$t"); fi
done

echo >&2
echo "==== ${#pass[@]}/$(( ${#pass[@]} + ${#fail[@]} )) passed ====" >&2
for t in "${pass[@]}"; do echo "  PASS $t" >&2; done
for t in "${fail[@]}"; do echo "  FAIL $t" >&2; done
[ "${#fail[@]}" -eq 0 ]
