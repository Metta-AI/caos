#!/usr/bin/env bash
# Runs INSIDE a testenv worker (tests/suite-in-caos), as ROOT. Generalizes
# test-in-caos from one rgrep run to a multi-worker SUITE inside a nested
# process-mode stack: it publishes an inner std from the worker binaries that
# arrived as /cas/args/bins (each as curry(dummy, bin) — the process-backend
# shape, no images), then drives the core computations — a file-count fold, a
# deep-deps DAG, an rgrep fold — through the inner stack. One job, keyed on
# (script, binaries): the tests-as-cached-jobs contract over the real engine
# (map-then/run-then promises, self-recursion, sparse trees), not just one
# worker.
set -euo pipefail

fail() {
  echo "SUITE-IN-CAOS FAIL: $*" >&2
  echo "--- inner server log ---" >&2; tail -25 /tmp/server.log 2>/dev/null >&2 || true
  exit 1
}

# The runner injects the OUTER run's std/salt; an inner client must not inherit
# them (it would name a std tree the inner server never saw).
unset CAOS_STD CAOS_SALT

caos get -r /cas/args/bins
mkdir -p /pt
cp /cas/args/bins/* /pt/
chmod +x /pt/*

INNER=http://127.0.0.1
cli() { CAOS_SERVER_URL=$INNER /pt/caos-cli "$@"; }

echo "== inner server (process backend, hermetic cache) =="
# Dead redis port: the worker's container is on the outer caos-net where
# caos-redis resolves, but a shared result cache maps request-hash ->
# object-hash and object presence is per-repo — a cross-stack hit would point
# at an object only the outer repo has. A dead port reads as a miss, so every
# result is computed in and pinned from THIS repo.
mkdir -p /tmp/inner-git
CAOS_GIT_DIR=/tmp/inner-git CAOS_IMAGE_RESOLVE=none CAOS_REDIS_ADDR=127.0.0.1:6399 \
  /pt/server >/tmp/server.log 2>&1 &
ok=""
for _ in $(seq 1 30); do
  if git ls-remote "$INNER" >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ -n "$ok" ] || fail "inner server never came up"

echo "== inner process-mode runnerd =="
CAOS_SERVER_URL=$INNER CAOS_RUNNER_MODE=process CAOS_RUNNER_ROOT=/tmp/slots \
  CAOS_PROCESS_CAOS=/pt/caos CAOS_PROCESS_WORKER=/pt/worker-runner \
  CAOS_RUNNER_SLOTS=6 /pt/runnerd >/tmp/runnerd.log 2>&1 &
sleep 1
grep -q "process slots" /tmp/runnerd.log || fail "runnerd not in process mode"

echo "== client repo + fixtures =="
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$INNER"
# A dummy tree to curry binaries onto (any tree works as the process-mode
# image; the trampoline reads `bin` and ignores it).
mkdir -p dummy && echo marker > dummy/m
# file-count fixture: 3 files across two levels.
mkdir -p fc/sub && echo 1 > fc/a && echo 2 > fc/b && echo 3 > fc/sub/c
# deep-deps fixture: a -> b,c ; b -> d ; c -> d ; d -> (none).
for p in a b c d; do mkdir -p "pkgs/$p"; done
printf 'b\nc\n' > pkgs/a/DEPS
printf 'd\n'    > pkgs/b/DEPS
printf 'd\n'    > pkgs/c/DEPS
: > pkgs/d/DEPS
# rgrep fixture.
mkdir -p rg/sub && printf 'a needle\nno\n' > rg/a.txt && printf 'none\n' > rg/b.txt \
  && printf 'deep needle\n' > rg/sub/c.txt
cp /pt/worker-file-count /pt/worker-dirs-only /pt/worker-deep-deps /pt/worker-rgrep .
git add -A
git commit -qm fixtures

echo "== publish inner std (curry each worker binary onto the dummy tree) =="
dummy=$(git rev-parse HEAD:dummy)
entries=""
for name in file-count dirs-only deep-deps rgrep; do
  h=$(cli curry "$dummy" -- "--bin:@=worker-$name")
  entries+="040000 tree ${h}"$'\t'"${name}"$'\n'
done
stdtree=$(printf '%s' "$entries" | git mktree)
git push -q --force caos "$stdtree:refs/caos/std" || fail "publishing inner std"
echo "  ok: inner std = $stdtree"

echo "== file-count: a 3-file fold =="
n=$(cli run /cas/std/file-count -- --in:@=fc)
[ "$n" = "3" ] || fail "file-count = $n, want 3"
one=$(cli run /cas/std/file-count -- --in:@=fc/a)
[ "$one" = "1" ] || fail "file-count of a leaf = $one, want 1"
echo "  ok: fold=3, leaf=1"

echo "== deep-deps: the a->{b,c}->d DAG =="
cli run /cas/std/deep-deps dd -- --mode=all --packages:@=pkgs
[ -e dd/a/DEEP-DEPS/b ] || fail "a should deep-depend on b"
[ -e dd/a/DEEP-DEPS/c ] || fail "a should deep-depend on c"
[ -e dd/b/DEEP-DEPS/d ] || fail "b should deep-depend on d"
diff -r dd/b/DEEP-DEPS/d dd/c/DEEP-DEPS/d >/dev/null || fail "shared d should be identical under b and c"
echo "  ok: DAG deepened, shared node identical"

echo "== rgrep: a sparse fold =="
cur=$(cli curry "$dummy" -- --bin:@=worker-rgrep --pattern=needle)
cli run "$cur" rgout -- --in:@=rg
grep -q '1:a needle' rgout/a.txt || fail "flat rgrep match missing"
grep -q '1:deep needle' rgout/sub/c.txt || fail "recursive rgrep match missing"
[ ! -e rgout/b.txt ] || fail "sparse tree carries a matchless file"
echo "  ok: flat + recursive matches, sparse"

echo "SUITE-IN-CAOS: ALL PASS" > /tmp/verdict
caos put /tmp/verdict /cas/out
