#!/bin/bash
# Cloud-environment SETUP SCRIPT for caos sessions (claude.ai/code).
#
# Paste into the environment's "Setup script" field. Runs ONCE as root on Ubuntu
# 24.04 before Claude Code launches; the filesystem is then snapshotted and
# later sessions start from it with this skipped. What survives is what is
# written to DISK -- anything merely RUNNING does not.
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

CAOS_SERVER_URL="${CAOS_SERVER_URL:-http://localhost:9090}"

# ---------------------------------------------------------------------------
# The client
# ---------------------------------------------------------------------------
# A download, not a build: the five-minute budget is only half the reason, since
# work done after the snapshot is never cached either. GitHub is already on the
# Trusted allowlist, so this needs no network-policy change.

repo="${CAOS_REPO:-Metta-AI/caos}"
if [ -n "${CAOS_VERSION:-}" ]; then
    installer="https://github.com/$repo/releases/download/$CAOS_VERSION/install.sh"
else
    installer="https://github.com/$repo/releases/latest/download/install.sh"
fi
# `--no-repo-files`: the client goes on PATH, the configuration goes user-level
# below, and the checkout is left exactly as it was found.
curl -fsSL "$installer" | bash -s -- --no-repo-files || true

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
      { "hooks": [ { "type": "command", "command": "git remote get-url caos >/dev/null 2>&1 || git remote add caos '$CAOS_SERVER_URL'" } ] }
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
