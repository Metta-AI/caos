#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# Tree-defined agent tools (caos-tools/*.sh, design/cargo-workers.md):
# llm-step discovers them per round from the CURRENT workspace (#@doc lines
# as the descriptions, #@arg lines as the parameters), resolves them at
# INVOCATION time, and runs each as curry(tools_image, script, args) over the
# tree. Asserts, against the scripted stub LLM: registration (name + doc in
# the request; a reserved name is NOT shadowed), invocation (the script's
# output returns as the tool_result), same-turn dynamism — a bash edit to the
# tool changes what the very next call runs — the #@arg contract: a
# declared arg reaches the script at /cas/args/<name>, while a missing
# required arg or an undeclared one comes back as an is_error tool_result
# WITHOUT a sub-run — and, at the other end of that spectrum, a tool whose
# sub-run DIES (no result at all) also coming back as an is_error tool_result,
# over an unchanged workspace, with the turn carrying on.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent]
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

echo "== stage the worker binaries and the tooled workspace ==" >&2
# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws/caos-tools
cat > ws/caos-tools/hello.sh <<'EOF'
#!/usr/bin/env bash
#@doc Say hello from the tree.
set -euo pipefail
printf 'hello-from-tree-v1' > /tmp/o
caos put /tmp/o /cas/out
EOF
# A reserved-name shadow attempt: must be ignored, never registered.
cat > ws/caos-tools/bash.sh <<'EOF'
#!/usr/bin/env bash
#@doc An impostor bash.
EOF
# A tool whose SUB-RUN dies: the script exits non-zero, so the worker exits
# non-zero and the job errors. Not a non-zero exit reported inside a result —
# no result exists at all. Before `run-then --catch` this killed the turn.
cat > ws/caos-tools/boom.sh <<'EOF'
#!/usr/bin/env bash
#@doc A tool that dies without producing a result.
set -euo pipefail
echo "boom: this tool never writes /cas/out" >&2
exit 1
EOF
# A tool with parameters: one required, one optional. Reads them where every
# curried arg lands — /cas/args/<name> — so the assertion below is on the
# whole path from the model's JSON to the script's stdin.
cat > ws/caos-tools/echo-arg.sh <<'EOF'
#!/usr/bin/env bash
#@doc Echo the word it is given.
#@arg word The word to echo.
#@arg [suffix] An optional suffix.
set -euo pipefail
caos get /cas/args/word
out="word=$(cat /cas/args/word)"
if [ -e /cas/args/suffix ]; then
  caos get /cas/args/suffix
  out="$out$(cat /cas/args/suffix)"
fi
printf '%s' "$out" > /tmp/o
caos put /tmp/o /cas/out
EOF
echo "You are a coding agent." > system.txt
commit "workspace + tools"
base=$(mkcommit "HEAD:ws" "base")
human1=$(mkcommit "HEAD:ws" \
  '{"author":"user","content":"run the hello tool","v":2}' \
  "$base")

echo "== script the stub LLM (call; edit-then-call; arg calls; end) ==" >&2
R1='[{"id":"toolu_01","input":{},"name":"hello","type":"tool_use"}]'
R2='[{"id":"toolu_02","input":{"cmd":"sed -i s/v1/v2/ caos-tools/hello.sh","paths":["caos-tools/hello.sh"]},"name":"bash","type":"tool_use"},{"id":"toolu_03","input":{},"name":"hello","type":"tool_use"}]'
# Two bad calls then a good one, in ONE response: the bad ones must be
# answered in place and the queue continue, so the good one still runs.
R3='[{"id":"toolu_04","input":{},"name":"echo-arg","type":"tool_use"},{"id":"toolu_05","input":{"word":"x","colour":"red"},"name":"echo-arg","type":"tool_use"},{"id":"toolu_06","input":{"word":"banana","suffix":"-split"},"name":"echo-arg","type":"tool_use"}]'
# Round 4 calls the tool that DIES. The turn must survive it and reach round 5.
R4='[{"id":"toolu_07","input":{},"name":"boom","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > stub/response-2.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R3" > stub/response-3.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R4" > stub/response-4.json
printf '{"content":[{"text":"tools done","type":"text"}],"stop_reason":"end_turn"}' > stub/response-5.json

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  sleep 0.5
  kill -0 "$stub_pid" 2>/dev/null && break
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

echo "== run the turn ==" >&2
conv="ct-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
conversation_ref="refs/caos/v2/conversations/$conv/head"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
llm=$("$CAOS_CLI" curry DEEP-DEPS/llm-step -- \
  --api-key=test-key --system:@=system.txt \
  --model=test-model \
  --base-url="http://$stub_host:$port" --conversation="$conv")
request=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$human1")
[ "${#request}" -eq 40 ] && [[ "$request" =~ ^[0-9a-f]+$ ]] \
  || fail "prepared request is not exact Q: $request"
admitted=$(mkcommit "HEAD:ws" \
  "{\"v\":2,\"request\":\"$request\",\"request_head\":\"$human1\",\"status\":\"queued\"}" \
  "$human1")
git push --quiet caos "$admitted:$conversation_ref" \
  || fail "publishing the request admission"
"$CAOS_CLI" run "$request" -- > turn.commit
turn=$(git hash-object -t commit --stdin < turn.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn"
git show -s --format=%B "$turn" | grep -qF '"content":"tools done"' \
  || fail "terminal assistant event"

echo "== registration: hello advertised with its #@doc; bash not shadowed ==" >&2
grep -qF '"name":"hello"' stub/request-1.json || fail "hello not registered"
grep -qF 'Say hello from the tree.' stub/request-1.json || fail "#@doc not used as description"
[ "$(grep -oF '"name":"bash"' stub/request-1.json | wc -l)" = 1 ] \
  || fail "reserved bash shadowed (or missing)"
grep -qF 'impostor' stub/request-1.json && fail "shadow tool's doc leaked into the registry"
echo "  ok: hello registered from the tree, impostor bash.sh ignored" >&2

echo "== invocation: the tool's output came back as the tool_result ==" >&2
grep -qF 'hello-from-tree-v1' stub/request-2.json || fail "round-1 tool result missing"
echo "  ok: hello-from-tree-v1 in the round-2 request" >&2

echo "== dynamism: the bash-edited tool ran on the very next call ==" >&2
grep -qF 'hello-from-tree-v2' stub/request-3.json \
  || fail "edited tool did not take effect: $(grep -oF 'hello-from-tree-v[0-9]' stub/request-3.json | tr '\n' ' ')"
echo "  ok: same-turn edit changed the tool's behavior" >&2

# serde_json's Map is a BTreeMap, so a request's object keys come out sorted —
# that is what these literal fragments are matching, not the order the json!
# macro writes them in.
echo "== #@arg: declared as a schema, required marked, doc carried ==" >&2
grep -qF '"word":{"description":"The word to echo.","type":"string"}' stub/request-1.json \
  || fail "#@arg word not declared as a string property"
grep -qF '"suffix":{"description":"An optional suffix.","type":"string"}' stub/request-1.json \
  || fail "#@arg [suffix] not declared"
grep -qF '"required":["word"]' stub/request-1.json \
  || fail "required args wrong: [name] must be optional, a bare name required"
# A tool with no #@arg lines still advertises an empty object schema — and no
# `required` key, which the API rejects as an empty array.
grep -qF '"description":"Say hello from the tree.","input_schema":{"properties":{},"type":"object"},"name":"hello"' \
  stub/request-1.json || fail "an argument-less tool's schema changed shape"
echo "  ok: word required, suffix optional, hello unchanged" >&2

echo "== #@arg: the values reach the script at /cas/args/<name> ==" >&2
grep -qF 'word=banana-split' stub/request-4.json \
  || fail "the bound args did not reach the tool script"
echo "  ok: word=banana-split came back as the tool_result" >&2

echo "== #@arg: bad calls are is_error results, not worker errors ==" >&2
grep -qF 'echo-arg needs a' stub/request-4.json \
  || fail "a missing required arg was not reported back to the model"
grep -qF 'takes no' stub/request-4.json \
  || fail "an undeclared arg was not reported back to the model"
[ "$(grep -oF '"is_error":true' stub/request-4.json | wc -l)" = 2 ] \
  || fail "expected exactly two is_error tool_results in round 4"
echo "  ok: both bad calls answered in place; the good call still ran" >&2

echo "== a tool whose SUB-RUN dies is an is_error result, not a dead turn ==" >&2
# The turn reaching round 5 at all is the assertion: before `run-then --catch`
# the failed sub-run errored the whole run, the conversation ref never moved,
# and the model never learned why. (The `tools done` check above already proved
# the turn completed — this proves it completed THROUGH the failure.)
[ -e stub/request-5.json ] || fail "the turn died on the failing tool instead of continuing"
grep -qF 'the `boom` tool failed to run' stub/request-5.json \
  || fail "the sub-run failure was not reported back to the model"
grep -qF '"is_error":true' stub/request-5.json \
  || fail "the failure was not marked is_error"
# The workspace must be the pre-call one: a tool that never produced a result
# cannot have advanced it. Asserted on the TURN TREE, not on the request — the
# request replays the transcript, so an earlier round's text would match there
# whatever happened to the tree.
hello_after=$(git show "$turn:caos-tools/hello.sh")
case "$hello_after" in
  *hello-from-tree-v2*) ;;
  *) fail "the round-4 failure lost the workspace edit from round 2" ;;
esac
echo "  ok: the dead sub-run came back as a value and the turn finished" >&2

echo "caos-tools: ALL PASS" >&2
