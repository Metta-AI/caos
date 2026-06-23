#!/usr/bin/env bash
# Integration test for the `rustc` worker builder (caos-worker-rustc).
#
# Proves the loop: a Rust source file in the CAS -> the builder compiles it
# (static musl, linking the vendored worker-common) and emits a git-docker worker
# image -> that image runs as an ordinary worker. Also checks that the build is
# memoized (rebuilding identical source is a cache hit) and that editing the
# source yields a different worker.
#
# Requires the dev daemons running (`tilt up`): the caos server :9090 (storage +
# compute), redis, registry — and a docker the compute server can reach.
set -euo pipefail
cd "$(dirname "$0")"

echo "building caos client + loading base/rustc images..." >&2
nix build .#client -o result-client
nix run .#load-caos-worker-base >/dev/null
nix run .#load-caos-worker-rustc >/dev/null
nix build .#caos-worker-base-docker -o result-base
caos=$PWD/result-client/bin/client

# Storage and compute are one server now — a single URL covers both.
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
CAS=$PWD/.caos-dev/rustc-cas
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
SRC=$(mktemp -d)
trap 'rm -rf "$CAS" "$SRC"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
misses_since() { docker logs --since "$1" caos-compute-server 2>&1 \
                   | grep -c "cache miss:" || true; }

# The worker-base git-docker image the produced workers extend; curried into the
# builder so callers only pass --src.
"$caos" import-image result-base "$CAS/base" >/dev/null
builder=$("$caos" curry docker://caos-worker-rustc:latest -- --base="$CAS/base")

# A trivial worker, defined in source: write a greeting to /cas/out.
greeter() {
  cat > "$SRC/worker.rs" <<RS
use std::fs;
use std::process::ExitCode;
use worker_common::{caos, path, run_worker, scratch};
fn main() -> ExitCode { run_worker("greeter", run) }
fn run() -> Result<(), String> {
    let out = scratch("out")?;
    fs::write(out.join("greeting"), "$1\n").map_err(|e| format!("write: {e}"))?;
    caos(["put", path(&out), "/cas/out"])
}
RS
}

build_and_run() { # <src-cas> <img-cas> <result-cas>
  "$caos" run "$builder" "$2" -- --src="$1" >/dev/null
  "$caos" run "$2" "$3" -- >/dev/null
  "$caos" get -r "$3" >/dev/null
}

echo "== Phase A: source -> worker image -> run ==" >&2
greeter "hello from a source-built worker"
"$caos" put "$SRC/worker.rs" "$CAS/src" >/dev/null
build_and_run "$CAS/src" "$CAS/img" "$CAS/result"
grep -q "source-built worker" "$CAS/result/greeting" \
  || fail "built worker did not produce the expected output"
echo "  ok: compiled from source, ran, produced expected output" >&2

echo "== Phase B: rebuilding identical source is a cache hit ==" >&2
# A hit means the build (and the whole compile) is skipped: 0 cache misses, and
# the cached result is by definition the same image.
sleep 1; since=$(date +%s)
"$caos" run "$builder" "$CAS/img2" -- --src="$CAS/src" >/dev/null
sleep 1
m=$(misses_since "$since")
[ "$m" -eq 0 ] || fail "rebuild of identical source should be a hit, saw $m misses"
echo "  ok: 0 cache misses (compile skipped)" >&2

echo "== Phase C: editing the source yields a different worker ==" >&2
# The new worker producing the new output is proof it's a distinct worker; the
# miss count is unreliable here (a warm cache from a prior run may already hold
# this build), so we assert on content.
greeter "a different greeting entirely"
"$caos" put "$SRC/worker.rs" "$CAS/src2" >/dev/null
build_and_run "$CAS/src2" "$CAS/img3" "$CAS/result3"
grep -q "different greeting" "$CAS/result3/greeting" \
  || fail "edited worker did not produce the new output"
grep -q "different greeting" "$CAS/result/greeting" \
  && fail "the new output leaked into the original worker's result"
echo "  ok: edited source rebuilt to a new, working worker" >&2

echo "ALL PASS" >&2
