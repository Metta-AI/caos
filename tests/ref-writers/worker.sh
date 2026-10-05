# Who may write a ref (design/ref-writers.md), end to end against the stack
# under test: its pre-receive hook, a client's signatures, and the run tokens
# jobs are granted.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
stage() { echo "== $* ==" >&2; }

test_id="$(date +%s%N)-$$-$RANDOM"
empty=$(git -c user.name=t -c user.email=t@caos commit-tree "$(git mktree </dev/null)" -m "ref-writers $test_id")

stage "an unproven push to anything but a content-named ref is refused"
if git push -q caos "$empty:refs/heads/ref-writers-$test_id" 2>/dev/null; then
  fail "the stack accepted an ungoverned branch"
fi
git push -q caos "$empty:refs/caos/req/$empty" || fail "a content-named ref was refused"

stage "a writer founds a namespace and writes it; a stranger cannot"
ns=$("$CAOS_CLI" namespace new "ref-writers $test_id") || fail "founding a namespace"
"$CAOS_CLI" ref-push "$empty" "refs/caos/w/$ns/branch" || fail "the founder could not write"
if git push -q caos "$empty:refs/caos/w/$ns/unsigned" 2>/dev/null; then
  fail "an unsigned push wrote a governed ref"
fi

# A second writer: another checkout, with a key of its own.
mkdir -p /tmp/stranger && cd /tmp/stranger
git init -q . && git remote add caos "$CAOS_SERVER_URL"
git config caos.ref-writer-key "$("$CAOS_CLI" ref-writer-key new 2>/dev/null)"
stranger=$("$CAOS_CLI" ref-writer-key show)
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$empty"
if "$CAOS_CLI" ref-push "$empty" "refs/caos/w/$ns/stranger" 2>/dev/null; then
  fail "a key that is not a writer wrote the namespace"
fi
cd - >/dev/null

stage "adding a writer lets them write; removing them stops them"
"$CAOS_CLI" writers add "$ns" "$stranger" stranger || fail "adding a writer"
"$CAOS_CLI" writers list "$ns" | grep -q "^$stranger" || fail "the new writer is not listed"
(cd /tmp/stranger && "$CAOS_CLI" ref-push "$empty" "refs/caos/w/$ns/stranger") \
  || fail "an added writer could not write"
"$CAOS_CLI" writers remove "$ns" "$stranger" || fail "removing a writer"
if (cd /tmp/stranger && "$CAOS_CLI" ref-push "$empty" "refs/caos/w/$ns/after" 2>/dev/null); then
  fail "a removed writer still wrote"
fi

stage "a job writes only the namespaces it asked for and was handed"
job() { # <output> <ref> [--writes=...]
  "$CAOS_CLI" run "$1" --base:@=DEEP-DEPS/worker-test --worker1:@=test/job.sh \
    "--ref=$2" "${@:3}" >/dev/null || fail "running the $1 job"
  cat "$1/verdict"
}
[ "$(job granted "refs/caos/w/$ns/job" "--writes=$ns")" = pushed ] \
  || fail "a job granted the namespace could not write it: $(cat granted/stderr)"
[ "$(job ungranted "refs/caos/w/$ns/job-without")" = refused ] \
  || fail "a job that asked for nothing wrote the namespace"
other=$("$CAOS_CLI" namespace new "ref-writers other $test_id") || fail "founding a second namespace"
[ "$(job elsewhere "refs/caos/w/$ns/job-elsewhere" "--writes=$other")" = refused ] \
  || fail "a job granted another namespace wrote this one"
[ "$(job list "refs/caos/w/$ns/writers" "--writes=$ns")" = refused ] \
  || fail "a job changed who writes"

echo "ref-writers: ALL PASS"
