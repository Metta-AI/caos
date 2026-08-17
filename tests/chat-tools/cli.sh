#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# The agent's TOOL SET, driven through the real `chat` verb with NO real API
# calls: the scripted llm-stub plays the LLM exactly as in tests/llm-step.
# Three turns, each one response of tool calls plus the round that carries
# their results back:
#
#   inline   write/read/edit/ls — five calls, ONE extra round, no sub-run
#   mixed    inline write -> bash sub-run -> inline edit, serial over one queue
#   grep     the sparse-tree fold: root, scoped, and an invalid pattern
#
# The three run in this order and depend on it: `mixed` edits over the tree
# `inline` produced, and `grep` searches for what both wrote and asserts they
# left the tree alone. That chain is why they are ONE test rather than three —
# splitting further would mean re-minting the earlier turns to search them.
#
# Split out of tests/chat-offline, which keeps the `chat` verb itself (and
# tests/chat-talk the `talk` verb): fifteen serial turns in one file made it
# the suite's critical path (design/faster-tests.md, "One test, three tests").
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
T0=$(date +%s%3N)
stage() { echo "== $* == [+$(( $(date +%s%3N) - T0 ))ms]" >&2; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}
remote_tip() { # <ref>
  local lines
  lines=$(git ls-remote --refs caos "$1")
  [ -n "$lines" ] || return 1
  [ "${lines#*$'\n'}" = "$lines" ] || return 1
  printf '%s\n' "${lines%%[[:space:]]*}"
}

stage "stage the worker binaries and fixtures"
# The agent workers are std SOURCE entries, not host binaries: chat resolves
# llm-step through the workspace's own DEPS and builds it via rustc
# (design/caos-expr.md, Phase 3), and llm-step's own `.caos-expr` binds the tool
# images this file exercises — bash-tool, rgrep, bash, merge. Only the LLM API
# stub is staged here, and only because the test needs a server to point the
# workers at.
# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

# The conversation's workspace, and the identity chat's human commits use. The
# `notes/` subtree is what the file tools write into and grep searches.
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace + worker binaries"
git config user.name tester
git config user.email tester@example.com
export ANTHROPIC_API_KEY=test-key

# The conversation base: a commit over just the ws tree (HEAD's tree here also
# carries the binaries and stub scripts).
base=$(mkcommit "HEAD:ws" "base")

stage "script the stub LLM (three turns, six rounds)"
mkdir stub

# Five calls in one response — four good, one bad edit — all inline: no bash
# sub-run, so the whole batch costs exactly one extra API round (request-2
# carries all five results). The bad edit must come back is_error, not fail
# the turn.
INLINE_CALLS='[
 {"id":"tu_w","input":{"file_path":"notes/new.txt","content":"hello world"},"name":"write","type":"tool_use"},
 {"id":"tu_r","input":{"file_path":"notes/new.txt"},"name":"read","type":"tool_use"},
 {"id":"tu_e","input":{"file_path":"notes/new.txt","old_string":"hello","new_string":"goodbye"},"name":"edit","type":"tool_use"},
 {"id":"tu_x","input":{"file_path":"notes/new.txt","old_string":"never there","new_string":"x"},"name":"edit","type":"tool_use"},
 {"id":"tu_l","input":{"path":"notes"},"name":"ls","type":"tool_use"}]'
printf '{"content":%s,"stop_reason":"tool_use"}' "$(printf '%s' "$INLINE_CALLS" | tr -d '\n')" > stub/response-1.json
printf '{"content":[{"text":"file tools done","type":"text"}],"stop_reason":"end_turn"}' > stub/response-2.json

# Order matters: bash must see the freshly written file, the edit must run on
# bash's output tree. Exercises drive -> launch -> callback -> drive.
MIXED_CALLS='[
 {"id":"tu_mw","input":{"file_path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file_path":"mix.txt","old_string":"hello","new_string":"world"},"name":"edit","type":"tool_use"}]'
printf '{"content":%s,"stop_reason":"tool_use"}' "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > stub/response-3.json
printf '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' > stub/response-4.json

# Three grep calls in one response: a root grep (fold fans out over the
# workspace, result rendered path:linenum:line), a scoped grep (input = the
# subtree, so its cache key is (subtree, pattern)), and an invalid pattern
# (caught by the precheck: is_error, no sub-run). Serial like bash; the
# workspace must be untouched by all three.
GREP_CALLS='[
 {"id":"tu_g1","input":{"pattern":"hello"},"name":"grep","type":"tool_use"},
 {"id":"tu_g2","input":{"pattern":"goodbye","path":"notes"},"name":"grep","type":"tool_use"},
 {"id":"tu_g3","input":{"pattern":"("},"name":"grep","type":"tool_use"}]'
printf '{"content":%s,"stop_reason":"tool_use"}' "$(printf '%s' "$GREP_CALLS" | tr -d '\n')" > stub/response-5.json
printf '{"content":[{"text":"grep done","type":"text"}],"stop_reason":"end_turn"}' > stub/response-6.json

# Start the stub on a free port; workers reach this host as
# host.containers.internal on the container network.
stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  # Wait for the LISTENER, not for a fixed interval: a flat `sleep 0.5` here
  # was half a second of a test whose whole body is a few seconds, and the stub
  # binds in a few ms. Probing the port also tells the two failures apart — a
  # dead process (retry on another port) against one still coming up.
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools"
ref="refs/caos/v2/conversations/$conv/head"
# Workers reach the stub as host.containers.internal from the outer engine's
# container network; nested siblings share this job's netns (CAOS_STUB_HOST).
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "inline file tools: write/read/edit/ls in ONE round trip"
"$CAOS_CLI" chat "$conv" -m "exercise the file tools" --base "$base" "${opts[@]}" > inline.out
sed 's/^/  inline| /' inline.out >&2
turn_inline=$(remote_tip "$ref") || fail "inline-tool conversation has no head"
git fetch -q caos "$turn_inline"
[ "$(git show "$turn_inline:notes/new.txt")" = "goodbye world" ] \
  || fail "write+edit did not land in the turn tree"
[ "$(git show "$turn_inline:notes/todo.txt")" = "hello notes" ] || fail "sibling file lost"
grep -qF "write notes/new.txt" inline.out || fail "write progress line missing"
grep -qF "read notes/new.txt" inline.out || fail "read progress line missing"
grep -qF "ls notes" inline.out || fail "ls progress line missing"
grep -qF '"hello world"' stub/request-2.json || fail "read result (pre-edit content) not sent"
grep -qF 'wrote notes/new.txt (11 bytes)' stub/request-2.json || fail "write result not sent"
grep -qF 'edited notes/new.txt (1 replacement)' stub/request-2.json || fail "edit result not sent"
grep -qF 'new.txt\ntodo.txt' stub/request-2.json || fail "ls listing not sent"
grep -qF '"is_error":true' stub/request-2.json || fail "bad edit not marked is_error"
grep -qF 'old_string not found' stub/request-2.json || fail "bad edit error not explained"
[ ! -f stub/request-3.json ] || fail "inline tools cost extra LLM rounds"
echo "  ok: five calls, one round trip, error as value, tree updated" >&2

stage "mixed queue: inline write -> bash sub-run -> inline edit"
"$CAOS_CLI" chat "$conv" -m "mix inline and bash" "${opts[@]}" > mixed.out
sed 's/^/  mixed| /' mixed.out >&2
turn_mixed=$(remote_tip "$ref") || fail "mixed-tool turn has no head"
git fetch -q caos "$turn_mixed"
[ "$(git show "$turn_mixed:mix.txt")" = "world" ] || fail "post-bash edit did not land"
[ "$(git show "$turn_mixed:mix3.txt")" = "HELLO" ] || fail "bash did not see the inline write"
# The request also replays earlier turns' tool_results, so assert this
# round's three results by id, in call order.
seq=$(grep -o '"tool_use_id":"tu_m[wbe]"' stub/request-4.json | grep -o 'tu_m[wbe]' | paste -sd,)
[ "$seq" = "tu_mw,tu_mb,tu_me" ] || fail "results missing or misordered: $seq"
[ ! -f stub/request-5.json ] || fail "unexpected extra LLM round"
echo "  ok: write -> bash -> edit, serial over one queue" >&2

stage "grep: sparse-tree fold — root, scoped, and invalid pattern"
"$CAOS_CLI" chat "$conv" -m "search the workspace" "${opts[@]}" > grep.out
sed 's/^/  grep| /' grep.out >&2
turn_grep=$(remote_tip "$ref") || fail "grep turn has no head"
git fetch -q caos "$turn_grep"
git diff --quiet "$turn_mixed" "$turn_grep" -- || fail "grep changed the workspace tree"
grep -qF "grep hello" grep.out || fail "root grep progress line missing"
grep -qF "grep goodbye notes" grep.out || fail "scoped grep progress line missing"
grep -qF 'notes/todo.txt:1:hello notes' stub/request-6.json || fail "root grep match not sent"
grep -qF 'notes/new.txt:1:goodbye world' stub/request-6.json || fail "scoped grep match not sent"
grep -qF '"is_error":true' stub/request-6.json || fail "invalid pattern not marked is_error"
grep -qF 'invalid pattern' stub/request-6.json || fail "invalid pattern error not explained"
[ ! -f stub/request-7.json ] || fail "unexpected extra LLM round"
echo "  ok: fold matches rendered, scope honored, bad pattern as value, tree untouched" >&2

stage "done"
echo "chat-tools: ALL PASS" >&2
