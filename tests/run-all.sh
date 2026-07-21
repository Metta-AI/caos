#!/usr/bin/env bash
# The caos test runner. Foldable tests run as per-test caos JOBS in a nested
# stack (design/cargo-workers.md) — cached, so an unchanged test is an instant
# hit and editing one test's fixtures re-runs only its job. The rest still run
# host-driven, until they're folded too (they need the full std inside the
# nested stack — the toolchain images — or their cli.sh stops shelling out to
# `nix`). There is no separate "test-all" test and no phase-demo tests: this IS
# the runner, and run-nested.sh (shipped into each job) is its one inner script.
#
# Usage: tests/run-all.sh          Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

# Tests that run as nested caos jobs today, by backend:
#   process  curry-able Rust bin-workers, on fast chroot slots
#   socket   image-based workers (the bash SCRIPT worker) via the engine socket
FOLD_PROCESS=(file-count dirs-only deep-deps rgrep)
FOLD_SOCKET=(symlinks untracked run-then)

echo "building caos client + bringing the stack up (once for the suite)..." >&2
nix build .#caos-cli -o result-caos || exit 1
export CAOS_CLI=$PWD/result-caos/bin/caos-cli
export CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
nix run .#caosd -- up >&2 || exit 1
export CAOS_STACK_READY=1
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"

pass=(); fail=()

# ---------------------------------------------------------------------------
# Nested batch: build the inner-stack pieces once, then fire one job per test.
# ---------------------------------------------------------------------------
echo "== preparing the nested-job stack (binaries + bash image) ==" >&2
CLIENT=$PWD/.caos-dev/run-all-client
rm -rf "$CLIENT"; mkdir -p "$CLIENT"; git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "http://localhost:9090"
git -C "$CLIENT" config gc.auto 0
trap 'rm -rf "$CLIENT" 2>/dev/null' EXIT

cp tests/lib/run-nested.sh "$CLIENT/run-nested.sh"

# Build the inner-stack binaries into a scratch dir (out-links must not land in
# the client repo), then copy just the binaries in.
BROOT=$(mktemp -d)
for attr in server runnerd caos worker-runner worker-file-count \
            worker-dirs-only worker-deep-deps worker-rgrep; do
  nix build ".#$attr" -o "$BROOT/$attr" || exit 1
done
mkdir -p "$CLIENT/bins"
cp -L "$BROOT"/server/bin/server "$BROOT"/runnerd/bin/runnerd \
  "$BROOT"/caos/bin/caos "$BROOT"/caos/bin/caos-cli "$BROOT"/worker-runner/bin/worker-runner \
  "$BROOT"/worker-file-count/bin/worker-file-count "$BROOT"/worker-dirs-only/bin/worker-dirs-only \
  "$BROOT"/worker-deep-deps/bin/worker-deep-deps "$BROOT"/worker-rgrep/bin/worker-rgrep "$CLIENT/bins/"

# The self-contained bash SCRIPT worker image for the socket cases (static
# setuid caos + the generic bash /worker), tagged in the host store; the inner
# server passes docker://<tag> through.
BASH_IMAGE=localhost/caos-bash-worker:latest
ictx=$(mktemp -d)
cp -L "$CLIENT"/bins/caos "$ictx/caos"
cat > "$ictx/worker" <<'WORKER'
#!/bin/bash
set -euo pipefail
caos get /cas/args/script
bash /cas/args/script
if [ ! -e /cas/out ]; then : > /tmp/caos-empty-out; caos put /tmp/caos-empty-out /cas/out; fi
WORKER
cat > "$ictx/Containerfile" <<EOF
FROM debian:stable-slim
COPY caos /bin/caos
COPY worker /worker
RUN chmod 4755 /bin/caos && chmod 0755 /worker
EOF
docker build -t "$BASH_IMAGE" "$ictx" >/tmp/run-all-imgbuild.log 2>&1 \
  || { tail -20 /tmp/run-all-imgbuild.log >&2; echo "bash image build failed" >&2; exit 1; }
rm -rf "$ictx"

mkdir -p "$CLIENT/cases"
for c in "${FOLD_PROCESS[@]}" "${FOLD_SOCKET[@]}"; do cp -r "tests/$c" "$CLIENT/cases/$c"; done
( cd "$CLIENT" && git add -A && git -c user.email=t@c -c user.name=c commit -qm setup )

fire() { # <name> <backend>
  local extra=(); [ "$2" = socket ] && extra=(--bash_image="$BASH_IMAGE")
  ( cd "$CLIENT" && "$CAOS_CLI" run /cas/std/testenv "out-$1" \
      -- --script:@=run-nested.sh --test:@="cases/$1" --bins:@=bins \
         --backend="$2" "${extra[@]}" )
}
nested() { # <name> <backend>
  echo "=== tests/$1 (nested:$2) ===" >&2
  if fire "$1" "$2" && grep -q "RUN-TEST: PASS" "$CLIENT/out-$1"; then
    pass+=("tests/$1"); else fail+=("tests/$1"); fi
}
for c in "${FOLD_PROCESS[@]}"; do nested "$c" process; done
for c in "${FOLD_SOCKET[@]}"; do nested "$c" socket; done

# ---------------------------------------------------------------------------
# Host-driven batch: everything else with a cli.sh (pending fold — step 2/3).
# ---------------------------------------------------------------------------
folded=" ${FOLD_PROCESS[*]} ${FOLD_SOCKET[*]} "
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
