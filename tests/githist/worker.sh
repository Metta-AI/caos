#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack (dev/cli-test stages the repo, then runs this).
#
# Exercises the three `@git` history tools — DEEP-DEPS/log-tool, DEEP-DEPS/show-tool,
# DEEP-DEPS/diff-tool. Each is handed `wc` (a workspace commit, as a gitlink) and
# reads history from that one entry point: `caos get-hash` for every object,
# no git binary in the image.
#
# A CLIENT TEST BECAUSE THE PRECONDITION NEEDS A CLIENT. The tools walk to a
# commit's parents, so the closure has to be on the server, and `--wc:commit=`
# is what puts it there (`ensure_pushed`). A worker cannot: `put-commit` writes
# to its own CAS, and a commit bound as `--x:@=` is a gitlink, which git
# reachability does not traverse — so a worker-minted ancestor is unreachable
# from the tool's container.
#
# THE FIXTURE, three commits deep so first-parent walking is exercised rather
# than assumed:
#
#   c1  hist/a.txt=one                      (root)
#   c2  hist/a.txt=two, hist/b.txt=added    subject "second"
#   c3  hist/a.txt=three                    subject "third"
#
# The salt rides in each commit MESSAGE, so a salted run really recomputes
# rather than replaying the reports it produced last time.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

SALT=""
if [ -n "${CAOS_TEST_SALT:-}" ]; then SALT=" ($CAOS_TEST_SALT)"; fi

# grep as a QUESTION, never as a statement: `cmd | grep -q x && fail` is an
# AND-list whose failure ends the script under `set -e`, so the PASSING case
# would end the test (CLAUDE.md, "Shell").
has() { printf '%s\n' "$2" | grep -q "$1"; }

# A tool run. `--wc:commit=` is the whole point: it passes the commit unpeeled
# and pushes its closure, which is what the tool then walks.
#
# The entry is DEEP-DEPS/<name>, not std/<name>: cli-test stages the test's
# declared dependencies at ./DEEP-DEPS in the client repo, and a client repo is
# not the caos tree.
#
# THE REPORT IS ON STDERR. `run-tool` puts the result's "<kind> <hash>" line on
# stdout and the tool's rendered report on stderr, plus its own `1126 <tool>:`
# note — so a plain $(...) captures the hash and none of the answer.
run_tool() { # <entry> <rev> [--k=v ...]
  local entry=$1 rev=$2 out; shift 2
  out=$("$CAOS_CLI" run-tool "$entry" --wc:commit="$rev" "$@" 2>&1 >/dev/null) || return 1
  printf '%s\n' "$out" | grep -v '^1126 ' || true
}

mkdir -p hist
printf 'one\n' > hist/a.txt
commit "first$SALT"
C1=$(git rev-parse HEAD)

printf 'two\n' > hist/a.txt
printf 'added\n' > hist/b.txt
commit "second$SALT"
C2=$(git rev-parse HEAD)

printf 'three\n' > hist/a.txt
commit "third$SALT"
C3=$(git rev-parse HEAD)
echo "== fixture: c1=${C1:0:12} c2=${C2:0:12} c3=${C3:0:12} ==" >&2

echo "== log: newest first, from the workspace commit ==" >&2
r=$(run_tool DEEP-DEPS/log-tool HEAD --count=3) || fail "log did not run"
printf '%s\n' "$r" >&2
[ "$(printf '%s\n' "$r" | grep -c .)" -eq 3 ] \
  || fail "log did not print exactly three commits"
[ "$(printf '%s\n' "$r" | grep . | head -1 | cut -d' ' -f1)" = "${C3:0:12}" ] \
  || fail "log did not start at HEAD ${C3:0:12}"
[ "$(printf '%s\n' "$r" | grep . | sed -n 2p | cut -d' ' -f1)" = "${C2:0:12}" ] \
  || fail "log's second line is not ${C2:0:12}"
has 'third' "$r" || fail "log lost the subject"
echo "  ok: three commits, newest first, with subjects" >&2

echo "== log --path: only commits that touched it ==" >&2
# hist/b.txt is added in c2 and untouched after, so exactly one commit qualifies.
r=$(run_tool DEEP-DEPS/log-tool HEAD --path=hist/b.txt --count=10) || fail "log --path did not run"
printf '%s\n' "$r" >&2
[ "$(printf '%s\n' "$r" | grep -c .)" -eq 1 ] \
  || fail "log --path=hist/b.txt did not narrow to one commit"
[ "$(printf '%s\n' "$r" | grep . | cut -d' ' -f1)" = "${C2:0:12}" ] \
  || fail "log --path=hist/b.txt named the wrong commit"
echo "  ok: one commit touched b.txt" >&2

echo "== show: the commit and the diff it introduced ==" >&2
r=$(run_tool DEEP-DEPS/show-tool HEAD) || fail "show did not run"
printf '%s\n' "$r" >&2
has '^commit  ' "$r" || fail "show has no commit line"
has '^parent  ' "$r" || fail "show has no parent line"
has 'third' "$r"     || fail "show lost the message"
# The diff c2..c3 introduced: hist/a.txt two -> three.
has '^-two' "$r"   || fail "show's diff lost the removed line"
has '^+three' "$r" || fail "show's diff lost the added line"
echo "  ok: commit, parent, message and its diff" >&2

echo "== show HEAD~2: walking back two first-parents ==" >&2
r=$(run_tool DEEP-DEPS/show-tool 'HEAD~2') || fail "show HEAD~2 did not run"
printf '%s\n' "$r" >&2
has 'first' "$r" || fail "show HEAD~2 is not the root commit"
echo "  ok: reached the root" >&2

echo "== diff: an explicit range, scoped to a path ==" >&2
r=$(run_tool DEEP-DEPS/diff-tool HEAD --from="$C1" --to=HEAD --path=hist/a.txt) \
  || fail "diff did not run"
printf '%s\n' "$r" >&2
has '^-one' "$r"   || fail "diff lost the base line"
has '^+three' "$r" || fail "diff lost the final line"
if has 'b.txt' "$r"; then fail "diff --path=hist/a.txt leaked b.txt"; fi
echo "  ok: c1..HEAD over a.txt, scoped" >&2

echo "PASS: log, show and diff read history with no git in the image" >&2
