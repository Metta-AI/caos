#!/bin/bash
# Cloud-environment SETUP SCRIPT for caos sessions (claude.ai/code).
#
# Runs ONCE as root on Ubuntu 24.04 before Claude Code launches; the filesystem
# is then snapshotted and later sessions start from it with this skipped. What
# survives is what is written to DISK -- anything merely RUNNING does not.
#
# DO NOT PASTE THIS FILE into the "Setup script" field. Paste these two lines,
# so that editing this file is enough and the settings form never has to be
# touched again:
#
#   B=https://raw.githubusercontent.com/Metta-AI/caos/main/dev/claude-code
#   curl -fsSL "$B/cloud/setup.sh" | bash -s -- --base="$B"
#
# `--base` is the whole configuration: everything else -- which repo, which
# branch or commit, where the sibling scripts live -- is read back out of it. A
# script piped into bash cannot see its own URL (no $0, no path, no referrer),
# so it has to be told once, and once is all it is told.
#
# Swap `main` for a branch or a commit sha to test a change: the setup script,
# the installer, the session hook and the client then ALL come from that one
# ref, and there is no second place to keep in step.
#
# NOTHING HERE TOUCHES A REPOSITORY. Everything is user-level configuration in
# the container, so one environment serves every repo and no project has to
# carry caos or Claude Code settings of its own. Three routes were possible and
# only this one works:
#
#   * repo `.claude/settings.json` -- works, but is a file in every repository
#   * managed settings -- ruled out: an Anthropic-hosted cloud session "doesn't
#     read a device's MDM profile or file"
#   * user-level settings written HERE -- measured, and what this uses
#
# The docs' line about user settings "staying on your machine" is about syncing
# yours upward, not about a file written in the container.
#
# THE HOME IS /root, measured: the setup script runs as root, the CLI runs as
# root, and hooks resolved $HOME to /root even though the repo sits at
# /home/user/repo and Claude's own state at /home/claude/.claude. All three are
# written anyway -- it costs nothing and the day that changes, this keeps
# working.
set -uo pipefail
export DEBIAN_FRONTEND=noninteractive

# ---------------------------------------------------------------------------
# The client
# ---------------------------------------------------------------------------
# A download, not a build: the five-minute budget is only half the reason, since
# work done after the snapshot is never cached either. GitHub is already on the
# Trusted allowlist, so this needs no network-policy change.

RAW="https://raw.githubusercontent.com"
base="$RAW/Metta-AI/caos/main/dev/claude-code"
for arg in "$@"; do
    case "$arg" in
        --base=*) base="${arg#--base=}"; base="${base%/}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# The repo and the ref come back out of the base, which is why there is only
# one thing to state. Shape-checked only: install.sh takes the same --base and
# does the real parse, so repeating that here would be a second rule to keep in
# step. The check is worth its four lines anyway -- a typo caught now names the
# typo, where the same typo caught later is a 404 on a URL nobody typed.
case "$base" in
    "$RAW"/*/*/*/dev/claude-code) ;;
    *)
        echo "FATAL: --base must look like" >&2
        echo "  $RAW/<owner>/<repo>/<ref>/dev/claude-code" >&2
        echo "  got: $base" >&2
        exit 1
        ;;
esac

# WHICH CLIENT: whatever --base names, passed straight through. There is no
# branch or version to choose here, because choosing one could only mean
# installing a client that does not match the scripts installing it.
#
# CAOS_IROH_TICKET is the one environment variable left, and could not be
# anything else: it is read at SESSION start, long after this has run and been
# snapshotted, so no argument here could carry it.
args="--no-repo-files --base=$base"
installer="$base/install.sh"

# `--no-repo-files`: the client goes on PATH, the configuration goes user-level
# below, and the checkout is left exactly as it was found.
#
# Checked afterwards rather than trusted: `curl -fsSL <404> | bash` exits ZERO.
# curl writes nothing, bash reads an empty script and succeeds, and the setup
# looks clean while installing nothing at all. The failure then surfaces one
# layer down as every hook dying on `caos: command not found`, which reads like
# a hook problem. Fail here, where the cause is still visible.
echo "installing the caos client from $installer $args" >&2
curl -fsSL "$installer" | bash -s -- $args
if ! command -v caos >/dev/null 2>&1; then
    echo "FATAL: the caos client did not install from $installer" >&2
    echo "  The installer's own error is above this line; read that, not this." >&2
    exit 1
fi
caos --version >&2 2>/dev/null || true

# ---------------------------------------------------------------------------
# dumbpipe, when the caos server is reached over iroh
# ---------------------------------------------------------------------------
# Only needed if CAOS_IROH_TICKET is set at session time, but installed
# unconditionally: this is the one place that can write to the snapshot, and a
# 4 MB binary is cheaper than discovering later that it is missing.
#
# PINNED, and not asked of api.github.com. That query was anonymous, so it is
# rate limited per IP and a cloud VM shares its address with every other cloud
# VM -- the same 403 that killed the client install, except this one fell back
# silently and would have gone on doing so until the pinned version rotted.
# A version bump here is a one-line edit; a mystery 403 is not.
dp_tag=v0.39.0
curl -fsSL "https://github.com/n0-computer/dumbpipe/releases/download/$dp_tag/dumbpipe-$dp_tag-linux-x86_64.tar.gz" \
    | tar xz -C /usr/local/bin ./dumbpipe || true
chmod 0755 /usr/local/bin/dumbpipe 2>/dev/null || true
# Not fatal -- CAOS_SERVER_URL alone is a valid arrangement -- but said out
# loud, because the alternative is discovering it at session start.
command -v dumbpipe >/dev/null 2>&1 \
    || echo "WARNING: dumbpipe did not install; CAOS_IROH_TICKET will not work" >&2

# The per-session work: the tunnel and the git remote. Fetched from the same
# ref as everything else rather than embedded here as a heredoc, which it was:
# a copy inside this file had to be re-synced by hand after every edit to the
# original, and a stale one would have installed the wrong hook while both
# files looked right in a diff.
if ! curl -fsSL "$base/cloud/session-start.sh" \
     -o /usr/local/bin/caos-cloud-session-start; then
    echo "FATAL: could not fetch $base/cloud/session-start.sh" >&2
    exit 1
fi
chmod 0755 /usr/local/bin/caos-cloud-session-start
bash -n /usr/local/bin/caos-cloud-session-start || {
    echo "FATAL: $base/cloud/session-start.sh does not parse" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Hooks and the tool server, user-level
# ---------------------------------------------------------------------------

for home in /root /home/claude /home/user; do
    [ -d "$home" ] || continue
    mkdir -p "$home/.claude"

    # The hooks are the recording. `caos cc hook` reads the event as JSON on
    # stdin and names its own event, so one command serves all of them.
    #
    # The SessionStart hook is also what makes this repo-independent: the client
    # finds caos through the `caos` git remote, and an arbitrary checkout has
    # none. Adding it at session start is per-repo configuration applied from
    # user-level settings, which is the whole point.
    cat > "$home/.claude/settings.json" <<EOF
{
  "permissions": {
    "deny": ["Read", "Write", "Edit", "NotebookEdit", "Bash", "Glob", "Grep"],
    "allow": ["mcp__caos"]
  },
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "caos-cloud-session-start" } ] }
    ],
    "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "caos cc hook" } ] } ],
    "PreToolUse": [
      { "matcher": "mcp__caos__.*", "hooks": [ { "type": "command", "command": "caos cc hook" } ] }
    ],
    "Stop":        [ { "hooks": [ { "type": "command", "command": "caos cc hook" } ] } ],
    "StopFailure": [ { "hooks": [ { "type": "command", "command": "caos cc hook" } ] } ]
  }
}
EOF
    chmod 0644 "$home/.claude/settings.json"

    # settings.json cannot declare an MCP server -- that lives in the user
    # config beside it. Merged with jq rather than overwritten: the file also
    # holds account state a cloud session put there.
    cfg="$home/.claude.json"
    [ -s "$cfg" ] || echo '{}' > "$cfg"
    tmp="$cfg.caos.$$"
    if jq '.mcpServers = ((.mcpServers // {}) + {caos: {type: "stdio", command: "caos", args: ["cc", "serve"]}})' \
         "$cfg" > "$tmp" 2>/dev/null; then
        cat "$tmp" > "$cfg"
    fi
    rm -f "$tmp"
done

exit 0
