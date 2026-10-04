#!/bin/bash
# tests/squash-layers — a WORKER test: no client, no repo.
#
# Drives std/squash-layers directly over the stack shape design/stacks.md
# describes: a base B at 00-base, layer 1 on B, layer 2 copied from layer 1,
# then layer 1 changes and is MERGED UP into layer 2, so layer 2's tip is a
# merge commit; a third layer sits on layer 2. Squashing must give one
# single-parent commit per layer that has a message file, B <- C1 <- C2 <- C3,
# each carrying its layer tip's tree, author and committer and its file's
# message byte for byte, returned as ONE TREE of gitlinks — and must refuse,
# as a report, every input the help says it refuses.
#
# Every fixture message carries `--test-salt`, so a salted run mints new commits
# and genuinely recomputes; the server's salt does not reach a sub-run's key
# (CLAUDE.md), so this payload is what does.
#
# STAGED: no run can be waited on, so each assertion is the `then` of the run
# it is about. The refusals are a table walked one case per stage.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --squash:@=/cas/args/squash "$@"; }

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)

mktree() { # <cas-name> <path=content>... -> its oid
  local dst=$1; shift
  local r=/tmp/t; rm -rf "$r"; mkdir -p "$r"
  local pair
  for pair in "$@"; do
    printf '%s\n' "${pair#*=}" > "$r/${pair%%=*}"
  done
  caos put "$r" "/cas/$dst" >/dev/null || fail "publishing tree $dst"
  caos hash "/cas/$dst"
}

# A DISTINCT time per commit, and a committer time distinct from the author's,
# so "the author and committer are the layer tip's" is a real check.
header() { # <tree-oid> <time> [parent...]
  local tree=$1 ts=$2; shift 2
  local p
  printf 'tree %s\n' "$tree"
  for p in "$@"; do printf 'parent %s\n' "$p"; done
  printf 'author dev <dev@caos> %s +0000\n' "$ts"
  printf 'committer dev <dev@caos> %s +0000\n\n' "$((ts + 50))"
}
mint() { # <cas-name> <tree-oid> <time> <message> [parent...]
  local dst=$1 tree=$2 ts=$3 msg=$4; shift 4
  { header "$tree" "$ts" "$@"; printf '%s (%s)\n' "$msg" "$SALT"; } > /tmp/commit
  caos put-commit /tmp/commit "/cas/$dst" || fail "minting $dst"
}

# A stack directory: each name a link to a /cas object — a minted commit makes
# a gitlink, a tree makes a plain directory.
mkstack() { # <cas-name> <entry=cas-name>...
  local dst=$1; shift
  local r=/tmp/s; rm -rf "$r"; mkdir -p "$r"
  local pair
  for pair in "$@"; do ln -s "/cas/${pair#*=}" "$r/${pair%%=*}"; done
  caos put "$r" "/cas/$dst" >/dev/null || fail "publishing stack $dst"
}

# The messages. Multi-line, the way an agent writes a real commit message: a
# body after a blank line, a paragraph break inside the body, and one file (M2)
# with no final newline, which is the one byte the tool supplies.
M1="Add the core ($SALT)

The parser core,
over two lines.
"
M2="Test the core ($SALT)

One body line, and no final newline."
M3="Document the core ($SALT)

First paragraph.

Second paragraph.
"
# A messages directory: each <entry>=<text> one file holding exactly <text>,
# and <entry>/ a subdirectory.
mkmessages() { # <cas-name> <entry=text>...
  local dst=$1; shift
  local r=/tmp/m; rm -rf "$r"; mkdir -p "$r"
  local pair
  for pair in "$@"; do
    case $pair in
      */) mkdir -p "$r/$pair"; printf 'x\n' > "$r/$pair/x" ;;
      *) printf '%s' "${pair#*=}" > "$r/${pair%%=*}" ;;
    esac
  done
  caos put "$r" "/cas/$dst" >/dev/null || fail "publishing messages $dst"
}

# Deterministic, so every stage re-mints the same oids rather than carrying them.
build() {
  B_T=$(mktree b-t "f.txt=base")
  B=$(mint b "$B_T" 1700000000 base)
  # Not an ancestor of anything: an `onto` no layer contains.
  OTHER=$(mint other "$B_T" 1700000050 "unrelated base")
  L1A_T=$(mktree l1a-t "f.txt=base" "a.txt=one")
  L1A=$(mint l1a "$L1A_T" 1700000100 "layer 1" "$B")
  L2A_T=$(mktree l2a-t "f.txt=base" "a.txt=one" "b.txt=two")
  L2A=$(mint l2a "$L2A_T" 1700000200 "layer 2" "$L1A")
  L1B_T=$(mktree l1b-t "f.txt=base" "a.txt=one, revised")
  L1B=$(mint l1b "$L1B_T" 1700000300 "revise layer 1" "$L1A")
  # What std/merge would produce merging L1B up into L2A: [ours, theirs].
  L2B_T=$(mktree l2b-t "f.txt=base" "a.txt=one, revised" "b.txt=two")
  L2B=$(mint l2b "$L2B_T" 1700000400 "merge layer 1" "$L2A" "$L1B")
  L3_T=$(mktree l3-t "f.txt=base" "a.txt=one, revised" "b.txt=two" "c.txt=docs")
  L3=$(mint l3 "$L3_T" 1700000500 "layer 3" "$L2B")
  # The third layer is `02_docs`, which sorts AFTER `02-tests` only in byte
  # order ('-' is 0x2d, '_' 0x5f); a dictionary collation, which ignores
  # punctuation, puts "02docs" first and the tool would refuse the stack. The
  # tool documents filename order under LC_ALL=C, and this pins it.
  mkstack merged 00-base=b 01-core=l1b 02-tests=l2b 02_docs=l3
  mkstack unmerged 00-base=b 01-core=l1b 02-tests=l2a
  mkstack plain 00-base=b 01-core=l1b notes=b-t
  mkmessages msgs "01-core=$M1" "02-tests=$M2" "02_docs=$M3"
  mkmessages msgs-2 "01-core=$M1" "02-tests=$M2"
  mkmessages msgs-extra "01-core=$M1" "03-docs=$M3"
  mkmessages msgs-plain "01-core=$M1" "notes=$M3"
  mkmessages msgs-empty-file "01-core=$M1" "02-tests="
  mkmessages msgs-dir "01-core=$M1" "02-tests/"
  mkmessages msgs-none
}

squash() { # <stack-cas-name> <messages-cas-name> [onto-cas-name] -> a request hash
  caos prepare-request --base:hash="$(caos hash /cas/args/squash)" \
    --stack:@="/cas/$1" --messages:@="/cas/$2" --onto:@="/cas/${3:-b}"
}

# The refusals: <stack> <messages> <onto> | the words the report must carry.
# Each is what the help promises to refuse, and each must arrive as a VALUE.
# After `build`, which every stage runs: a case names its fixtures by /cas name,
# and those are this worker's own.
build
REFUSALS=(
  "unmerged msgs-2 b|does not contain|02-tests|01-core|Merge"
  "merged msgs-2 other|does not contain|01-core|the base $OTHER"
  "merged msgs-extra b|stack has no entry 03-docs"
  "plain msgs-plain b|stack entry notes is not a source gitlink"
  "merged msgs-none b|messages has no files"
  "merged msgs-empty-file b|messages/02-tests is empty"
  "merged msgs-dir b|messages/02-tests is not a file"
)
launch_refusal() { # <case index>
  local spec=${REFUSALS[$1]%%|*} stack msgs onto
  read -r stack msgs onto <<< "$spec"
  echo "== refusal $1: stack=$stack messages=$msgs onto=$onto ==" >&2
  caos run-request-then "$(squash "$stack" "$msgs" "$onto")" \
    --then:hash="$(next "refused-$1")"
}

case "$stage" in

start)
  echo "== squash a stack whose layer 2 merged a revised layer 1 ==" >&2
  caos run-request-then "$(squash merged msgs)" --then:hash="$(next squashed)"
  ;;

squashed)
  caos get /cas/args/result >/dev/null
  [ "$(caos kind /cas/args/result)" = tree ] \
    || fail "the result is not a tree: $(cat /cas/args/result)"
  if [ -e /cas/args/result/report ]; then
    caos get /cas/args/result/report >/dev/null
    fail "the squash was refused: $(cat /cas/args/result/report)"
  fi
  # 00-base has no message file, so it is not published; nothing else rides
  # along. LC_ALL=C so this listing is the byte order the tool promises.
  got=$(cd /cas/args/result && LC_ALL=C; printf '%s ' *)
  [ "$got" = "01-core 02-tests 02_docs " ] \
    || fail "the result tree holds [$got], not exactly the three layers"

  # The commits the tool must mint, minted here: each layer tip's tree on the
  # one below, with that tip's author and committer (every tip has its own
  # time) and the layer's message byte for byte, plus the one final newline M2
  # lacks. Equal oids check all of it at once, and since nothing else goes in,
  # that the same input mints the same commits.
  { header "$L1B_T" 1700000300 "$B"; printf '%s' "$M1"; } > /tmp/c1
  C1=$(caos put-commit /tmp/c1 /cas/c1) || fail "minting c1"
  { header "$L2B_T" 1700000400 "$C1"; printf '%s\n' "$M2"; } > /tmp/c2
  C2=$(caos put-commit /tmp/c2 /cas/c2) || fail "minting c2"
  { header "$L3_T" 1700000500 "$C2"; printf '%s' "$M3"; } > /tmp/c3
  C3=$(caos put-commit /tmp/c3 /cas/c3) || fail "minting c3"
  for pair in "01-core=$C1" "02-tests=$C2" "02_docs=$C3"; do
    entry=${pair%%=*} want=${pair#*=}
    [ "$(caos kind "/cas/args/result/$entry")" = commit ] \
      || fail "result entry $entry is not a gitlink"
    [ "$(caos hash "/cas/args/result/$entry")" = "$want" ] \
      || fail "result entry $entry is $(caos hash "/cas/args/result/$entry"), not $want"
  done
  echo "  ok: {01-core: C1, 02-tests: C2, 02_docs: C3}, gitlinks, B <- C1 <- C2 <- C3" >&2
  launch_refusal 0
  ;;

refused-*)
  i=${stage#refused-}
  [ -n "${REFUSALS[$i]+x}" ] || fail "no refusal case $i"
  caos get /cas/args/result >/dev/null
  [ -d /cas/args/result ] || fail "refusal $i is not a report: $(cat /cas/args/result)"
  [ -e /cas/args/result/report ] \
    || fail "refusal $i was not refused: the result is $(cd /cas/args/result && printf '%s ' *)"
  caos get /cas/args/result/report >/dev/null
  report=$(cat /cas/args/result/report)
  [[ "$report" == FAILED:* ]] || fail "refusal $i has no FAILED banner: $report"
  IFS='|' read -r -a words <<< "${REFUSALS[$i]}"
  for word in "${words[@]:1}"; do
    [[ "$report" == *"$word"* ]] || fail "refusal $i does not say \"$word\": $report"
  done
  echo "  ok: $report" >&2
  if [ "$((i + 1))" -lt "${#REFUSALS[@]}" ]; then
    launch_refusal "$((i + 1))"
  else
    printf 'squash-layers: ALL PASS\n' > /tmp/report
    cat /tmp/report >&2
    caos put /tmp/report /cas/out
  fi
  ;;

*) fail "unknown --stage: $stage" ;;
esac
