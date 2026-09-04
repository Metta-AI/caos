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

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    log "not a git repository; nothing to point at $server"
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
