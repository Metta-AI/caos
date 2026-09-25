#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
# tests/llm-eval-path — a WORKER test, in dev/worker-test (it needs git).
#
# `eval_path` answers "what does this path BUILD?": it applies every
# `.caos-expr` from a root down to the path and reports the `<kind> <hash>` of
# the object that falls out. A worker cannot evaluate, so the step tail-calls the
# server's `eval-path-then` — the same route `tool_help` takes to a tool's image,
# with `--catch`, so a path that cannot be evaluated is a value the model reads
# rather than a dead turn.
#
# THE HASH HAS TO BE USABLE, and that is the other half of the subject: `read`,
# `ls` and `grep` each take a `root`, and this asserts the root may be ANY object
# the server holds — including a build product that appears at no path in any
# tree — rather than only another revision of the conversation.
#
# Every call is declared by the test and run in `--tools-only` mode, exactly as
# `caos mcp` runs one. Not a stylistic choice: each call's `root` is the PREVIOUS
# call's answer, and a scripted stub can only replay a batch written before the
# batch ran.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

# Declare one call the way a harness that drives the model itself does, and
# leave it PENDING. `$head` and `$round` follow the declaration.
declare_call() {
  local call=$1 tool=$2 input=$3 output key value
  round=""
  output=$($TOOL declare --repo /tmp/repo --head "$head" --request "$request" \
    --actor tester --id "$call" --tool "$tool" --input "$input" \
    --ref "$conversation_ref") || fail "declaring $call"
  while read -r key value; do
    case "$key" in
      head) head=$value ;;
      round) round=$value ;;
    esac
  done <<<"$output"
  assert_oid "$head" "the declared conversation head"
  [ -n "$round" ] || fail "declare did not report a round"
}

# Run the step for one declared call, in its own fresh ArgTree naming the
# admitted request by `--run`. `sub-run` starts the job and returns, so the
# call's own record is what says it is done.
run_tools_only() {
  local call=$1 arg_tree attempt status
  arg_tree=$(caos prepare-request --base:hash="$llm" --head:commit="$human" \
    --run="$request" --tools-only="$call") \
    || fail "preparing the tools-only run for $call"
  caos sub-run "$arg_tree" >/dev/null || fail "dispatching the tools-only step for $call"
  for ((attempt = 0; attempt < 900; attempt++)); do
    head=$($TOOL fetch --repo /tmp/repo --ref "$conversation_ref") \
      || fail "fetching the conversation after $call"
    head=${head#head }
    assert_oid "$head" "the conversation head after $call"
    status=$($TOOL tools --repo /tmp/repo --head "$head" --request "$request" \
      | jq -r --arg id "$call" 'select(.id == $id) | .status') || status=""
    # `failed` is a TERMINAL outcome carrying a tool_result the model reads, not
    # a dead turn: an evaluation that cannot be done is recorded that way -- the
    # same status `tool_help` on a path that is not there gets. So the CALLER
    # decides whether it was expected, and $CALL_STATUS is how.
    case "$status" in
      complete | failed) CALL_STATUS=$status; return 0 ;;
      cancelled) fail "call $call ended cancelled" ;;
    esac
    sleep 0.2
  done
  fail "call $call never completed"
}

# One call end to end, leaving its tool_result text in $TEXT and whether it was
# an error in $IS_ERROR. An `is_error` result is a COMPLETE call, so the wait
# above cannot tell the two apart and this is what does.
do_call() {
  local id=$1 tool=$2 input=$3
  declare_call "$id" "$tool" "$input"
  run_tools_only "$id"
  $TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
    --round "$round" --id "$id" > /tmp/observation.json \
    || fail "$id recorded no observation"
  TEXT=$(jq -r '.content[0].text' /tmp/observation.json) \
    || fail "$id's observation carries no text"
  IS_ERROR=$(jq -r '.is_error // false' /tmp/observation.json) \
    || fail "$id's observation is malformed"
}

# `eval_path`, expecting an answer: leaves the reported kind in $KIND and the
# object in $HASH. The first line is the `<kind> <hash>` shape `caos-cli
# eval-path` prints, so what the model is told is what a human can paste back.
eval_object() {
  local id=$1 input=$2 first
  do_call "$id" eval_path "$input"
  [ "$CALL_STATUS" = complete ] || fail "$id: eval_path ended $CALL_STATUS"
  [ "$IS_ERROR" = false ] || fail "$id: eval_path failed: $TEXT"
  first=${TEXT%%$'\n'*}
  KIND=${first%% *}
  HASH=${first##* }
  assert_oid "$HASH" "$id's evaluated object"
  case "$TEXT" in
    *"root=$HASH"*) ;;
    *) fail "$id did not say where its hash goes: $TEXT" ;;
  esac
}

stage "stage a source tree with one built directory and one plain one"
llm_test_setup

# The image the fixture's expression runs on. A fixture source tree holds no std
# to name by path, so it names the image by `:hash=` — as tests/caos-tools does.
bash_img=$(caos hash /cas/args/bash)

rm -rf /tmp/ws
mkdir -p /tmp/ws/plain /tmp/ws/build
printf 'source, not a product\n' > /tmp/ws/plain/note.txt
cat > /tmp/ws/build/build.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
rm -rf /tmp/product
mkdir -p /tmp/product/logs
printf 'BUILD OK\n' > /tmp/product/report.txt
printf 'eval-path-needle: the build log\n' > /tmp/product/logs/build.log
caos put /tmp/product /cas/out
EOF
chmod +x /tmp/ws/build/build.sh
# A `run`, so evaluating this directory DISPATCHES a job and its value is that
# job's result — a tree that exists at no path in any source tree, which is
# precisely the thing a model otherwise has no way to name.
printf 'run --base:hash=%s --worker1:@=build.sh --in:@=.\n' "$bash_img" \
  > /tmp/ws/build/.caos-expr

plain=$(publish_tree /tmp/ws/plain /cas/plain "publishing the plain directory")
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# EMPTY on purpose, like tests/llm-tools-only: a tools-only run must never reach
# the model, so a step that does finds no scripted response and fails.
rm -rf /tmp/stub
mkdir -p /tmp/stub
start_stub /tmp/stub

new_llm_conversation eval-path "$STUB_PORT" "$ws"
admit_turn "show me what this tree builds"

stage "a bare source-tree name is that tree's own root"
# The prefix that SELECTS the tree is the whole path, so nothing is left to
# evaluate within it — and the fixture's root carries no `.caos-expr`, so the
# tree is its own value. Both facts in one assertion: the answer is `$ws`.
eval_object eval-main '{"path":"main"}'
[ "$KIND" = tree ] || fail "main evaluated to a $KIND, not a tree"
[ "$HASH" = "$ws" ] \
  || fail "main did not evaluate to the source tree itself: $HASH vs $ws"
echo "  ok: main -> $HASH" >&2

stage "a directory with no expression evaluates to itself"
eval_object eval-plain '{"path":"main/plain"}'
[ "$HASH" = "$plain" ] \
  || fail "main/plain did not evaluate to its own tree: $HASH vs $plain"
echo "  ok: main/plain -> $HASH" >&2

stage "a directory that declares a build evaluates to the built thing"
eval_object eval-build '{"path":"main/build"}'
[ "$KIND" = tree ] || fail "main/build evaluated to a $KIND, not a tree"
product=$HASH
[ "$product" != "$ws" ] && [ "$product" != "$plain" ] \
  || fail "main/build returned a source tree rather than a product: $product"
echo "  ok: main/build -> $product" >&2

stage "the product is a root for ls, read and grep"
# THE POINT OF THE TOOL. This tree is named by nothing: it is not a revision of
# the conversation and sits at no path, so before `eval_path` a model had no way
# to ask what a build produced.
do_call ls-product ls "{\"root\":\"$product\"}"
[ "$IS_ERROR" = false ] || fail "ls refused the product root: $TEXT"
case "$TEXT" in
  *"logs/"*) ;;
  *) fail "ls of the product did not list its logs directory: $TEXT" ;;
esac
case "$TEXT" in
  *report.txt*) ;;
  *) fail "ls of the product did not list report.txt: $TEXT" ;;
esac

do_call read-product read "{\"root\":\"$product\",\"file-path\":\"report.txt\"}"
[ "$IS_ERROR" = false ] || fail "read refused the product root: $TEXT"
case "$TEXT" in
  *"BUILD OK"*) ;;
  *) fail "read of the product's report returned $TEXT" ;;
esac

do_call read-nested read "{\"root\":\"$product\",\"file-path\":\"logs/build.log\"}"
[ "$IS_ERROR" = false ] || fail "read refused a nested path under the product: $TEXT"
case "$TEXT" in
  *eval-path-needle*) ;;
  *) fail "read of the product's nested log returned $TEXT" ;;
esac

do_call grep-product grep "{\"root\":\"$product\",\"pattern\":\"eval-path-needle\"}"
[ "$IS_ERROR" = false ] || fail "grep refused the product root: $TEXT"
case "$TEXT" in
  *"logs/build.log:1:"*) ;;
  *) fail "grep of the product did not report a match with its path: $TEXT" ;;
esac
echo "  ok: ls, read and grep all accept a product hash as root" >&2

stage "a product hash is a root for eval_path too, and a blob reads directly"
eval_object eval-report "{\"root\":\"$product\",\"path\":\"report.txt\"}"
[ "$KIND" = blob ] || fail "a file inside the product evaluated to a $KIND, not a blob"
do_call read-blob read "{\"root\":\"$HASH\"}"
[ "$IS_ERROR" = false ] || fail "read refused a blob root with no file-path: $TEXT"
case "$TEXT" in
  *"BUILD OK"*) ;;
  *) fail "reading the blob root returned $TEXT" ;;
esac

eval_object eval-logs "{\"root\":\"$product\",\"path\":\"logs\"}"
[ "$KIND" = tree ] || fail "a subtree of the product evaluated to a $KIND, not a tree"
do_call ls-logs ls "{\"root\":\"$HASH\"}"
[ "$IS_ERROR" = false ] || fail "ls refused the product's subtree: $TEXT"
case "$TEXT" in
  *build.log*) ;;
  *) fail "ls of the product's logs subtree returned $TEXT" ;;
esac
echo "  ok: a product hash roots a further evaluation" >&2

stage "no path and no root is the conversation tree"
eval_object eval-root '{}'
[ "$KIND" = tree ] || fail "the conversation root evaluated to a $KIND, not a tree"
do_call ls-conversation ls "{\"root\":\"$HASH\"}"
[ "$IS_ERROR" = false ] || fail "ls refused the conversation tree: $TEXT"
# `main/` is the source tree's gitlink and `.caos/` the protocol's own directory:
# together they say the default root really is the CONVERSATION tree, not the
# source tree inside it.
case "$TEXT" in
  *"main/"*) ;;
  *) fail "the default root is not the conversation tree: $TEXT" ;;
esac
case "$TEXT" in
  *".caos/"*) ;;
  *) fail "the default root does not carry the protocol directory: $TEXT" ;;
esac
echo "  ok: the default root is the conversation tree" >&2

stage "an unevaluatable path and an unknown root are values, not dead turns"
# `is_error` is the assertion, not the record's status: the two errors are
# detected in different places -- the walk itself, which `--catch` delivers as a
# FAILED call the way a tool's own resolution failure is, and the worker's own
# lookup of the root, which never starts a walk and so completes -- and what
# matters either way is that the model was handed something it can act on.
do_call eval-missing eval_path '{"path":"main/nope"}'
[ "$IS_ERROR" = true ] || fail "a path that is not there was not an error: $TEXT"
case "$TEXT" in
  *"no such path"*) ;;
  *) fail "the missing path was not explained: $TEXT" ;;
esac
do_call eval-bad-root eval_path \
  '{"root":"0000000000000000000000000000000000000000"}'
[ "$IS_ERROR" = true ] || fail "an unknown root was not an error: $TEXT"
case "$TEXT" in
  *0000000000000000000000000000000000000000*) ;;
  *) fail "the unknown root error does not name the root: $TEXT" ;;
esac
echo "  ok: both came back as tool_results the model can act on" >&2

stage "nothing here called the model, and the spine is intact"
assert_spine "$head"
[ ! -f /tmp/stub/request-1.json ] || fail "a tools-only run called the model"

pass llm-eval-path
