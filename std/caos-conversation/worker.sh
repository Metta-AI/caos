#!/usr/bin/env bash
# The `caos-conversation` tool's worker. Its DOCS live in the sibling
# `.caos-expr` here-string, not in this header (SPEC, "Tools").
#
# It reads a recorded conversation by the hash of its tip commit, and prints it
# as text for an agent that is reviewing how a session went.
#
# WHERE A CONVERSATION LIVES (design/chat.md, v3/events.rs, v3/paths.rs):
#   - the TIP'S TREE holds the transcript: `.caos/transcript/<n>-<id>.json`, one
#     entry per message, and beside it `<n>-<id>/args-<call>.json`, the arguments
#     of the tool calls that message declared.
#   - the COMMIT MESSAGES hold everything about execution, on the first-parent
#     chain: `<kind>\n\n{"events":[...]}`. A tool's result is NOT in the tree. It
#     is a `payload` event (`{path, bytes:[<u8>...]}`, the bytes as a JSON array of
#     numbers) plus a `tool` event whose `result.observation` names that path.
# So this walks the chain root-ward with `caos get-hash` (a worker fetches any
# object by hash from the same git the conversations are recorded in), gathers
# the events oldest-first, and reads the transcript out of the tip's tree.
#
# EVERY OUTCOME IS THE VALUE, as in caos-test-result: a bad hash comes back as
# text the caller can correct, never as a job error.
set -euo pipefail

out=/tmp/out
: > "$out"
say() { printf '%s\n' "$*" >> "$out"; }
finish() { caos put "$out" /cas/out; exit 0; }

optarg() { # <name> <default>
  if [ -e "/cas/args/$1" ]; then
    caos get "/cas/args/$1"
    tr -d '[:space:]' < "/cas/args/$1"
  else
    printf '%s' "$2"
  fi
}

caos get /cas/args/hash
hash=$(tr -d '[:space:]' < /cas/args/hash)
if ! printf '%s' "$hash" | grep -qE '^[0-9a-f]{40}$'; then
  say "not a hash: $hash"
  say ""
  say "Pass the 40-character hash of the commit at the tip of a conversation."
  finish
fi
want_call=$(optarg call "")
width=$(optarg width 300)
case "$width" in ''|*[!0-9]*) width=300 ;; esac

# ---- walk the first-parent chain, tip to root ------------------------------
mkdir -p /tmp/ev /tmp/pay
n=0
cur=$hash
while :; do
  if ! caos get-hash "$cur" "/cas/c$n" 2>/tmp/fetch.err; then
    if [ "$n" -eq 0 ]; then
      say "no object $hash on this server:"
      say ""
      cat /tmp/fetch.err >> "$out"
      finish
    fi
    say "(history walk stopped: could not fetch $cur)"
    break
  fi
  if [ ! -f "/cas/c$n" ] || ! head -n 1 "/cas/c$n" | grep -q '^tree '; then
    if [ "$n" -eq 0 ]; then
      say "$hash is not a commit."
      say ""
      say "Pass the hash of a conversation's tip commit, not of a tree or a file."
      finish
    fi
    say "(history walk stopped: $cur is not a commit)"
    break
  fi
  if [ "$n" -eq 0 ]; then cp /cas/c0 /tmp/tip.raw; fi
  # The image has no sed or awk. A header line starting `parent ` precedes the
  # message, and no message line does (a message is `<kind>`, a blank line, then
  # one line of JSON), so the first match is the first parent.
  parent=$(grep -m1 '^parent ' "/cas/c$n" | cut -d' ' -f2 || true)
  # The event JSON is the message's last line.
  evfile="/tmp/ev/$(printf '%06d' "$n").json"
  tail -n 1 "/cas/c$n" > "$evfile"
  if ! jq -e . "$evfile" > /dev/null 2>&1; then
    printf 'commit %s: no readable event record; message ends: %s\n' "$cur" \
      "$(tail -n 3 "/cas/c$n" | head -c 300 | tr '\n' '|')" >> /tmp/notes
    : > "$evfile"
  fi
  n=$((n + 1))
  if [ -z "$parent" ]; then break; fi
  cur=$parent
  if [ "$n" -ge 200000 ]; then say "(history walk stopped after $n commits)"; break; fi
done

# Oldest first, so a later record of the same call replaces its earlier one.
printf '%s\n' /tmp/ev/*.json | xargs cat | jq -s '[reverse[] | .events[]?]' > /tmp/events.json

# Each payload's bytes are a JSON array of numbers. Emit them as `\0ooo` escapes
# and let printf's %b turn them back into the exact bytes.
while IFS=$'\t' read -r p esc; do
  printf '%b' "$esc" > "/tmp/pay/${p//\//__}"
done < <(jq -r '.[] | select(.event == "payload") | .value
  | .path + "\t" + ([.bytes[] | "\\0" + (((. / 64) | floor) | tostring)
      + ((((. / 8) | floor) % 8) | tostring) + ((. % 8) | tostring)] | join(""))' /tmp/events.json)

jq '[.[] | select(.event == "tool") | .value] | reduce .[] as $c ({}; .[$c.id] = $c)' \
  /tmp/events.json > /tmp/calls.json
jq '[.[] | select(.event == "request") | .value] | reduce .[] as $r ({}; .[$r.id] = $r)' \
  /tmp/events.json > /tmp/requests.json

# ---- the tip's tree: title and transcript ----------------------------------
tree=$(grep -m1 '^tree ' /tmp/tip.raw | cut -d' ' -f2)
caos get-hash "$tree" /cas/tree
title=""
if [ -e /cas/tree/.caos ]; then
  caos get /cas/tree/.caos
  if [ -e /cas/tree/.caos/title ]; then
    caos get /cas/tree/.caos/title
    title=$(cat /cas/tree/.caos/title)
  fi
  if [ -e /cas/tree/.caos/transcript ]; then
    caos get -r /cas/tree/.caos/transcript
  fi
fi

# ---- rendering -------------------------------------------------------------
trunc() { # <width> <call id>; text on stdin
  jq -Rrs --argjson w "$1" --arg id "$2" \
    'rtrimstr("\n") | if $w > 0 and length > $w
       then .[0:$w] + "… [" + ((length - $w) | tostring) + " more chars; pass call=" + $id + " to see all]"
       else . end'
}
indent() { jq -Rr '"      " + .'; }

payload_text() { # <payload path> -> the text a tool_result carries
  local f="/tmp/pay/${1//\//__}"
  if [ ! -f "$f" ]; then
    printf '(payload %s is not in the recorded history)' "$1"
    return
  fi
  jq -r '
    if type == "object" and has("content") then
      (if .is_error == true then "[is_error]\n" else "" end)
      + (.content | if type == "string" then . else
          map(if .type == "text" then .text else "[" + (.type // "?") + " block]" end) | join("\n") end)
    else tojson end' "$f" 2>/dev/null || cat "$f"
}

render_call() { # <id> <name> <argument path> <width>
  local id=$1 name=$2 argpath=$3 w=$4 status kind ref text
  status=$(jq -r --arg id "$id" '.[$id].status // "no result recorded"' /tmp/calls.json)
  say "  [call $id] $name -> $status"
  if [ -f "/cas/tree/$argpath" ]; then
    if [ "$w" -eq 0 ]; then
      say "    args:"
      jq . "/cas/tree/$argpath" | indent >> "$out"
    else
      say "    args: $(jq -c . "/cas/tree/$argpath" | trunc "$w" "$id")"
    fi
  else
    say "    args: (not in the transcript: $argpath)"
  fi
  kind=$(jq -r --arg id "$id" '.[$id].result.kind // empty' /tmp/calls.json)
  ref=$(jq -r --arg id "$id" '.[$id].result | (.observation // .error // .reason // empty)' /tmp/calls.json 2>/dev/null || true)
  if [ -n "$ref" ]; then
    if [ -f "/tmp/pay/${ref//\//__}" ]; then text=$(payload_text "$ref"); else text=$ref; fi
    say "    result${kind:+ ($kind)}:"
    printf '%s\n' "$text" | trunc "$w" "$id" | indent >> "$out"
  fi
}

render_entries() { # <width> [<only call id>]
  local w=$1 only=${2:-} f base ord role round block id name argpath p
  for f in /cas/tree/.caos/transcript/*.json; do
    [ -e "$f" ] || continue
    base=$(basename "$f" .json)
    ord=$((10#${base%%-*}))
    role=$(jq -r '.role' "$f")
    round=$(jq -r 'if .round == null then "" else " (round \(.round))" end' "$f")
    if [ -n "$only" ]; then
      jq -e --arg id "$only" 'any(.blocks[]; .type == "tool_use" and .id == $id)' "$f" > /dev/null || continue
    fi
    say "[$ord] ${role^^}$round"
    while IFS= read -r block; do
      case "$(jq -r '.type' <<< "$block")" in
        text)
          if [ -z "$only" ]; then jq -r '.text' <<< "$block"; fi >> "$out"
          ;;
        payload)
          p=$(jq -r '.path' <<< "$block")
          if [ -z "$only" ]; then
            if [ -f "/cas/tree/$p" ]; then cat "/cas/tree/$p" >> "$out"; else say "[payload block: $p]"; fi
          fi
          ;;
        tool_use)
          id=$(jq -r '.id' <<< "$block")
          if [ -n "$only" ] && [ "$id" != "$only" ]; then continue; fi
          name=$(jq -r '.name' <<< "$block")
          argpath=$(jq -r '.arguments.path' <<< "$block")
          render_call "$id" "$name" "$argpath" "$w"
          ;;
      esac
    done < <(jq -c '.blocks[]' "$f")
    say ""
  done
}

if [ -n "$want_call" ]; then
  if jq -e --arg id "$want_call" 'has($id)' /tmp/calls.json > /dev/null; then
    render_entries 0 "$want_call"
  else
    say "no call $want_call in this conversation."
    say ""
    say "Calls it recorded (ids as the default listing prints them):"
    jq -r 'keys[] | "  " + .' /tmp/calls.json >> "$out"
  fi
  finish
fi

entries=$(ls /cas/tree/.caos/transcript/*.json 2>/dev/null | wc -l || true)
say "Conversation${title:+: $title}"
say "tip $hash ($n commits walked, $entries transcript entries)"
say "calls: $(jq -r 'if length == 0 then "none" else ([.[] | .status] | group_by(.) | map("\(.[0]) \(length)") | join(", ")) end' /tmp/calls.json)"
say ""
say "NOT RECORDED: the model's text and reasoning between tool calls, calls to tools"
say "that are not caos's (and calls that never reached one), and timings. Gaps in the"
say "story below may be unrecorded steps."
say ""
if [ -s /tmp/notes ]; then
  say "Commits skipped while reading the history:"
  while IFS= read -r line; do say "  $line"; done < /tmp/notes
  say ""
fi
say "----"
render_entries "$width"

# Requests that did not end cleanly, with the reason the record kept.
bad=$(jq -r '[.[] | select(.status == "failed" or .status == "cancelling" or .status == "running" or .status == "queued" or .interrupted == true)] | .[] | "\(.id) \(.status)\(if .interrupted == true then " interrupted" else "" end) \(.error // "")"' /tmp/requests.json)
if [ -n "$bad" ]; then
  say "----"
  say "Requests that did not end idle:"
  while IFS=' ' read -r rid rstatus rest; do
    detail=""
    for word in $rest; do
      if [ -f "/tmp/pay/${word//\//__}" ]; then detail=$(payload_text "$word"); else detail=$word; fi
    done
    say "  $rid: $rstatus${detail:+ — $detail}"
  done <<< "$bad"
fi
finish
