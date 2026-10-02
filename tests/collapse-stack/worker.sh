#!/bin/bash
# tests/collapse-stack — a WORKER test: no client, no repo.
#
# Drives std/collapse-stack directly over the stack shape design/agent-github.md
# describes: a base B, layer 1 on B, layer 2 copied from layer 1, then layer 1
# changes and is MERGED UP into layer 2, so layer 2's tip is a merge commit.
# Collapsing must give one single-parent commit per layer, B <- C1 <- C2, each
# carrying its layer tip's tree and author, and must refuse a layer that has
# not merged the one below it.
#
# Every fixture message carries `--test-salt`, so a salted run mints new commits
# and genuinely recomputes; the server's salt does not reach a sub-run's key
# (CLAUDE.md), so this payload is what does.
#
# THREE STAGES: no run can be waited on, so each assertion is the `then` of the
# run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --collapse:@=/cas/args/collapse "$@"; }

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
mint() { # <cas-name> <tree-oid> <time> <message> [parent...]
  local dst=$1 tree=$2 ts=$3 msg=$4; shift 4
  local p
  { printf 'tree %s\n' "$tree"
    for p in "$@"; do printf 'parent %s\n' "$p"; done
    printf 'author dev <dev@caos> %s +0000\n' "$ts"
    printf 'committer dev <dev@caos> %s +0000\n' "$((ts + 50))"
    printf '\n%s (%s)\n' "$msg" "$SALT"
  } > /tmp/commit
  caos put-commit /tmp/commit "/cas/$dst" || fail "minting $dst"
}

# A stack directory: each name a gitlink to a minted commit.
mkstack() { # <cas-name> <entry=cas-name>...
  local dst=$1; shift
  local r=/tmp/s; rm -rf "$r"; mkdir -p "$r"
  local pair
  for pair in "$@"; do ln -s "/cas/${pair#*=}" "$r/${pair%%=*}"; done
  caos put "$r" "/cas/$dst" >/dev/null || fail "publishing stack $dst"
}

# Deterministic, so every stage re-mints the same oids rather than carrying them.
build() {
  B_T=$(mktree b-t "f.txt=base")
  B=$(mint b "$B_T" 1700000000 base)
  L1A_T=$(mktree l1a-t "f.txt=base" "a.txt=one")
  L1A=$(mint l1a "$L1A_T" 1700000100 "layer 1" "$B")
  L2A_T=$(mktree l2a-t "f.txt=base" "a.txt=one" "b.txt=two")
  L2A=$(mint l2a "$L2A_T" 1700000200 "layer 2" "$L1A")
  L1B_T=$(mktree l1b-t "f.txt=base" "a.txt=one, revised")
  L1B=$(mint l1b "$L1B_T" 1700000300 "revise layer 1" "$L1A")
  # What std/merge would produce merging L1B up into L2A: [ours, theirs].
  L2B_T=$(mktree l2b-t "f.txt=base" "a.txt=one, revised" "b.txt=two")
  L2B=$(mint l2b "$L2B_T" 1700000400 "merge layer 1" "$L2A" "$L1B")
  mkstack merged 01-core=l1b 02-tests=l2b
  mkstack unmerged 01-core=l1b 02-tests=l2a
}

LAYERS="01-core Add the core ($SALT)
02-tests Test the core ($SALT)"
collapse() { # <stack-cas-name> [extra arg...] -> a request hash
  local stack=$1; shift
  caos prepare-request --base:hash="$(caos hash /cas/args/collapse)" \
    --stack:@="/cas/$stack" --onto:@=/cas/b --layers="$LAYERS" "$@"
}
result() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }

case "$stage" in

start)
  build
  echo "== collapse a stack whose layer 2 merged a revised layer 1 ==" >&2
  caos run-request-then "$(collapse merged)" --then:hash="$(next collapsed)"
  ;;

collapsed)
  build
  # The commits the tool must mint, minted here: each layer tip's tree on the
  # one below, with that tip's author and committer (every tip has its own
  # time) and the layer's message. Equal oids check all of it at once, and
  # since nothing else goes in, that the same input mints the same commits.
  C1=$(mint c1 "$L1B_T" 1700000300 "Add the core" "$B")
  C2=$(mint c2 "$L2B_T" 1700000400 "Test the core" "$C1")
  want="01-core $C1
02-tests $C2"
  [ "$(result)" = "$want" ] || fail "expected:
$want
got:
$(result)"
  echo "  ok: base <- C1 <- C2, each with its layer tip's tree, author and committer" >&2

  echo "== a layer that has not merged the one below is refused ==" >&2
  caos run-request-then "$(collapse unmerged)" --then:hash="$(next refused)"
  ;;

refused)
  caos get /cas/args/result >/dev/null
  [ -d /cas/args/result ] || fail "the refusal is not a report: $(cat /cas/args/result)"
  caos get /cas/args/result/report >/dev/null
  report=$(cat /cas/args/result/report)
  [[ "$report" == FAILED* ]] || fail "no FAILED banner: $report"
  [[ "$report" == *01-core* && "$report" == *02-tests* ]] \
    || fail "the refusal does not name both layers: $report"
  echo "  ok: $report" >&2

  printf 'collapse-stack: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
