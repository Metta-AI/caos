#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# Tree-defined agent tools (caos-tools/*.sh, design/cargo-workers.md):
# llm-step discovers them per round from the CURRENT workspace (#@doc lines
# as the descriptions), resolves them at INVOCATION time, and runs each as
# curry(tools_image, script) over the tree. Asserts, against the scripted
# stub LLM: registration (name + doc in the request; a reserved name is NOT
# shadowed), invocation (the script's output returns as the tool_result),
# and same-turn dynamism — a bash edit to the tool changes what the very
# next call runs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent]
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

echo "== stage the worker binaries and the tooled workspace ==" >&2
cp "$CAOS_BIN_DIR/worker-llm-step" llm-step-bin
cp "$CAOS_BIN_DIR/worker-bash-tool" bash-tool-bin
stub_bin=$CAOS_BIN_DIR/llm-stub

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
echo "You are a coding agent." > system.txt
commit "workspace + tools"
base=$(mkcommit "HEAD:ws" "base")
human1=$(mkcommit "HEAD:ws" "run the hello tool" "$base")

echo "== script the stub LLM (call; edit-then-call; end) ==" >&2
R1='[{"id":"toolu_01","input":{},"name":"hello","type":"tool_use"}]'
R2='[{"id":"toolu_02","input":{"cmd":"sed -i s/v1/v2/ caos-tools/hello.sh","paths":["caos-tools/hello.sh"]},"name":"bash","type":"tool_use"},{"id":"toolu_03","input":{},"name":"hello","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > stub/response-2.json
printf '{"content":[{"text":"tools done","type":"text"}],"stop_reason":"end_turn"}' > stub/response-3.json

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
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
bash_tool=$("$CAOS_CLI" curry /cas/std/runner -- --bin:@=bash-tool-bin)
tools_img=$("$CAOS_CLI" curry /cas/std/bash --)
llm=$("$CAOS_CLI" curry /cas/std/runner -- --bin:@=llm-step-bin \
  --api_key=test-key --system:@=system.txt --bash_image="$bash_tool" \
  --tools_image="$tools_img" --model=test-model \
  --base_url="http://$stub_host:$port" --conversation="$conv")
"$CAOS_CLI" run "$llm" -- --head:commit="$human1" > turn.commit
turn=$(git hash-object -t commit --stdin < turn.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn"
[ "$(git show -s --format=%s "$turn")" = "tools done" ] || fail "turn message"

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

echo "caos-tools: ALL PASS" >&2
