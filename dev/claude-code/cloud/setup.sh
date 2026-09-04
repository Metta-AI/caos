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

# ---------------------------------------------------------------------------
# The client
# ---------------------------------------------------------------------------
# A download, not a build: the five-minute budget is only half the reason, since
# work done after the snapshot is never cached either. GitHub is already on the
# Trusted allowlist, so this needs no network-policy change.

# WHICH BUILD, all from the environment so one setup script serves every case
# and nothing has to be edited between runs:
#
#   CAOS_COMMIT=C    that commit's build          (wins over CAOS_BRANCH)
#   CAOS_BRANCH=X    the newest build on branch X
#   neither          the newest build on main
#   CAOS_VERSION=T   release T outright           (wins over both)
#
# The INSTALLER comes from the same place as the binary -- raw from git at that
# branch or commit. Otherwise a branch would get its own binary driven by
# somebody else's install logic, which is the confusing half of a version skew.

repo="${CAOS_REPO:-Metta-AI/caos}"
args="--no-repo-files"
if [ -n "${CAOS_VERSION:-}" ]; then
    installer="https://github.com/$repo/releases/download/$CAOS_VERSION/install.sh"
else
    ref="${CAOS_COMMIT:-${CAOS_BRANCH:-main}}"
    installer="https://raw.githubusercontent.com/$repo/$ref/dev/claude-code/install.sh"
    if [ -n "${CAOS_COMMIT:-}" ]; then
        args="$args --commit=$CAOS_COMMIT"
    else
        args="$args --branch=${CAOS_BRANCH:-main}"
    fi
fi

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
    echo "  A release has to EXIST for the binary to come from anywhere." >&2
    echo "  Check: curl -sI $installer" >&2
    exit 1
fi
caos --version >&2 2>/dev/null || true

# ---------------------------------------------------------------------------
# dumbpipe, when the caos server is reached over iroh
# ---------------------------------------------------------------------------
# Only needed if CAOS_IROH_TICKET is set at session time, but installed
# unconditionally: this is the one place that can write to the snapshot, and a
# 4 MB binary is cheaper than discovering later that it is missing.
dp_tag="$(curl -fsSL https://api.github.com/repos/n0-computer/dumbpipe/releases/latest 2>/dev/null \
    | jq -r '.tag_name // empty')"
dp_tag="${dp_tag:-v0.39.0}"
curl -fsSL "https://github.com/n0-computer/dumbpipe/releases/download/$dp_tag/dumbpipe-$dp_tag-linux-x86_64.tar.gz" \
    | tar xz -C /usr/local/bin ./dumbpipe || true
chmod 0755 /usr/local/bin/dumbpipe 2>/dev/null || true
# Not fatal -- CAOS_SERVER_URL alone is a valid arrangement -- but said out
# loud, because the alternative is discovering it at session start.
command -v dumbpipe >/dev/null 2>&1 \
    || echo "WARNING: dumbpipe did not install; CAOS_IROH_TICKET will not work" >&2

# The per-session work: the tunnel and the git remote. A script rather than an
# inline hook command, because it is too long to read inside JSON.
install -m 0755 /dev/stdin /usr/local/bin/caos-cloud-session-start <<'SESSIONSTART'
#!/bin/bash
# Installed by the cloud setup script as /usr/local/bin/caos-cloud-session-start
# and called from the user-level SessionStart hook. Runs at the start of every
# session, so it is idempotent and quiet when there is nothing to do.
#
# Two jobs, both per-session because neither survives the environment snapshot:
#
#   1. Bring up the iroh tunnel, when CAOS_IROH_TICKET names one.
#   2. Point this checkout's `caos` remote at whatever server results.
#
# The second is what makes the whole arrangement repo-independent: the client
# finds caos through a `caos` git remote, an arbitrary clone has none, and this
# adds it from user-level configuration rather than from anything committed.
set -uo pipefail

log() { printf 'caos: %s\n' "$*" >&2; }

port="${CAOS_TUNNEL_PORT:-19090}"
server="${CAOS_SERVER_URL:-}"

# Liveness is an HTTP ROUND TRIP, never a TCP connect. `dumbpipe connect-tcp`
# binds its local port before it has reached anything, and it accepts and then
# silently drops connections when the far node is gone -- so a bound port reads
# as "tunnel up" for a tunnel that carries nothing. A stale ticket presents
# exactly that way, and the first caos job then hangs for minutes instead of
# failing here. Any status code counts: a reply at all proves a live server.
reachable() {
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
        "http://127.0.0.1:$1/info/refs?service=git-upload-pack" 2>/dev/null)"
    [ -n "$code" ] && [ "$code" != 000 ]
}

# ---------------------------------------------------------------------------
# The tunnel
# ---------------------------------------------------------------------------
# A ticket names an iroh NODE, and the node id is the identity -- it comes from
# the listener's IROH_SECRET, so a ticket keeps working across restarts of the
# listener even though the address embedded in it goes stale. That is why one
# ticket can live in the environment indefinitely.

if [ -n "${CAOS_IROH_TICKET:-}" ]; then
    # A live tunnel first: a resumed session may already have one, and then it
    # does not matter whether dumbpipe is anywhere.
    if reachable "$port"; then
        log "tunnel already up on :$port"
    elif ! command -v dumbpipe >/dev/null 2>&1; then
        log "CAOS_IROH_TICKET is set but dumbpipe is not installed"
    else
        # A dumbpipe holding the port without serving anything would make the
        # new one fail to bind and the failure would be attributed to iroh.
        pkill -f "connect-tcp --addr 127.0.0.1:$port " 2>/dev/null

        log "opening the iroh tunnel on :$port"
        (dumbpipe connect-tcp --addr "127.0.0.1:$port" "$CAOS_IROH_TICKET" \
            >/tmp/caos-tunnel.log 2>&1 &)
        # Bounded wait: the first tool call would otherwise race the tunnel and
        # fail with a connection error that says nothing about why.
        for _ in $(seq 1 20); do
            reachable "$port" && break
            sleep 1
        done
        if reachable "$port"; then
            log "tunnel up"
        else
            log "tunnel did not reach a caos server; see /tmp/caos-tunnel.log"
        fi
    fi
    : "${server:=http://127.0.0.1:$port}"
fi

# ---------------------------------------------------------------------------
# The remote
# ---------------------------------------------------------------------------

if [ -z "$server" ]; then
    log "no CAOS_SERVER_URL and no CAOS_IROH_TICKET; leaving the remote alone"
    exit 0
fi

# The repo is named, not assumed from cwd. A hook's working directory is not
# contractually the project -- in a cloud session the checkout is at
# /home/user/repo while $HOME resolves to /root -- and a wrong cwd here does not
# error, it silently adds the remote to some other repository or to none, and
# the failure only shows up much later as a client that cannot find a server.
if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -d "$CLAUDE_PROJECT_DIR" ]; then
    cd "$CLAUDE_PROJECT_DIR" || exit 0
fi

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    log "$PWD is not a git repository; nothing to point at $server"
    exit 0
fi

# An existing remote is left alone: a checkout that already names a caos server
# has been set up deliberately, and repointing it from the environment would
# silently move someone's work to a different stack.
if current="$(git remote get-url caos 2>/dev/null)"; then
    if [ "$current" != "$server" ]; then
        log "caos remote already set to $current; leaving it (wanted $server)"
    fi
else
    git remote add caos "$server" && log "caos remote -> $server"
fi

exit 0
SESSIONSTART

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
