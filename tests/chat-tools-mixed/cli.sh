#!/usr/bin/env bash
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI):
#
# The subject here is the tested client's OWN `chat` tool-execution engine —
# specifically how it drains a queue of tool_use blocks that arrive together in
# a SINGLE model turn. The stub returns one turn carrying three heterogeneous
# calls at once (an inline `write`, a `bash` compute sub-run, then an inline
# `edit`), and the property pinned down is entirely internal to the client:
#
#   1. The client executes the queued calls IN ORDER, not concurrently and not
#      reordered — so each call observes the workspace effect of the ones before
#      it. `mix3.txt` == "HELLO" proves the bash sub-run saw the inline write's
#      file; `mix.txt` == "world" proves the trailing edit applied on top of it.
#   2. Inline filesystem tools and a real bash sub-run interleave through the
#      same dispatch path and commit to the same conversation tree.
#   3. The client returns the three tool_result blocks back to the model in the
#      original call order (tu_mw,tu_mb,tu_me) in ONE follow-up round, with no
#      spurious extra model round (no request-3.json).
#
# None of this exists outside the tested client: it is the client that parses
# the model's tool_use blocks, sequences them, runs the bash tool against the
# inner stack, folds the results into the workspace/commit, and reissues the
# request. There is no lower-level artifact to inspect instead — the ordering
# and interleaving ARE the behaviour under test — so $CAOS_CLI is essential,
# not incidental. A stub LLM stands in for the model precisely so the test can
# assert on what the CLI does with a fixed, mixed tool queue.
#
# The ordering assertion is the point; it needs no prior turns.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and scripted mixed-tool turn"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir ws
echo "fixture" > ws/original.txt
commit "workspace"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" base)

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file_path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file_path":"mix.txt","old_string":"hello","new_string":"world"},"name":"edit","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools-mixed"
conversation_ref="refs/caos/v2/conversations/$conv/head"
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "inline write, bash, and inline edit stay ordered"
"$CAOS_CLI" chat "$conv" -m "mix inline and bash" \
  --base "$base" "${opts[@]}" > mixed.out
while IFS= read -r line; do echo "  mixed| $line" >&2; done < mixed.out
turn=$(remote_tip "$conversation_ref") || fail "mixed-tool conversation has no head"
git fetch -q caos "$turn"
[ "$(git show "$turn:mix.txt")" = "world" ] || fail "post-bash edit did not land"
[ "$(git show "$turn:mix3.txt")" = "HELLO" ] || fail "bash did not see inline write"
sequence=$(grep -o '"tool_use_id":"tu_m[wbe]"' stub/request-2.json \
  | grep -o 'tu_m[wbe]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me" ] \
  || fail "results missing or misordered: $sequence"
[ ! -f stub/request-3.json ] || fail "unexpected extra model round"

stage "done"
echo "chat-tools-mixed: ALL PASS" >&2
