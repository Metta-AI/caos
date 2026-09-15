#!/bin/bash
# THROWAWAY PROBE -- paste into a cloud environment's "Setup script" field.
#
# Question: can a setup script configure hooks for EVERY repo, without each one
# committing `.claude/settings.json`?
#
# Repo settings are already proven to work, but they mean a file in every
# repository. Managed settings are ruled out: an Anthropic-hosted cloud session
# "doesn't read a device's MDM profile or file". That leaves USER-level settings
# written inside the container, which the docs neither promise nor exclude --
# the line about user settings "staying on your machine" is about syncing yours,
# not about a file written here.
#
# WHICH HOME is also unknown: the repo is at /home/user/repo while Claude's own
# state is under /home/claude/.claude. So every candidate gets a settings file
# whose hook names the home it came from, and the winner identifies itself.
set -uo pipefail

mkdir -p /tmp/hookprobe
chmod 1777 /tmp/hookprobe

for home in /home/claude /home/user /root; do
    [ -d "$home" ] || continue
    mkdir -p "$home/.claude"
    cat > "$home/.claude/settings.json" <<EOF
{
  "hooks": {
    "SessionStart":     [ { "hooks": [ { "type": "command", "command": "printf 'SessionStart from $home %s\\\\n' \"\$(date -Is)\" >> /tmp/hookprobe/markers" } ] } ],
    "UserPromptSubmit": [ { "hooks": [ { "type": "command", "command": "printf 'UserPromptSubmit from $home %s\\\\n' \"\$(date -Is)\" >> /tmp/hookprobe/markers" } ] } ],
    "PreToolUse":       [ { "hooks": [ { "type": "command", "command": "printf 'PreToolUse from $home %s\\\\n' \"\$(date -Is)\" >> /tmp/hookprobe/markers" } ] } ]
  }
}
EOF
    chmod 0644 "$home/.claude/settings.json"
    echo "wrote $home/.claude/settings.json" >> /tmp/hookprobe/setup.log
done

# Which homes existed at setup time, for the report.
{ echo "--- setup ran as $(id -un) at $(date -Is)"; ls -ld /home/* /root 2>/dev/null; } >> /tmp/hookprobe/setup.log

exit 0
