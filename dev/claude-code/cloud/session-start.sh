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

base=""
for arg in "$@"; do
    case "$arg" in
        --base=*) base="${arg#--base=}" ;;
    esac
done

port="${CAOS_TUNNEL_PORT:-19090}"
server="${CAOS_SERVER_URL:-}"

# Which environment is this? Printed first, every session, so that a stale
# snapshot announces itself instead of being mistaken for a broken fix.
if [ -r /usr/local/share/caos/setup-stamp ]; then
    while IFS= read -r line; do log "env $line"; done \
        < /usr/local/share/caos/setup-stamp
else
    log "no setup stamp; this environment predates it"
fi

# ---------------------------------------------------------------------------
# The client, refreshed
# ---------------------------------------------------------------------------
# The setup script installed one, into a filesystem that is then FROZEN. A push
# does not reach an existing environment, so a client left where the setup put
# it is as old as the environment is, no matter how many sessions start.
#
# Re-running the installer costs one `git ls-remote`, because it stops as soon
# as it finds the build already installed. Not fatal on failure: a working
# client that is out of date beats no session at all.
if [ -n "$base" ]; then
    if ! curl -fsSL "$base/install.sh" | bash -s -- --no-repo-files --base="$base"; then
        log "could not refresh the client; carrying on with the installed one"
    fi
    # AND THE CONFIGURATION, because it pins the commit the client was built
    # from. A refreshed client left with the old configuration would drive a
    # step from a different tree than itself, which is the one pairing that
    # cannot be allowed to go quiet. Writes nothing when nothing moved.
    if ! curl -fsSL "$base/cloud/configure.sh" | bash -s -- --base="$base"; then
        log "could not refresh the session configuration; carrying on"
    fi
fi

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

# ---------------------------------------------------------------------------
# The remote, THEN the tunnel
# ---------------------------------------------------------------------------
# ORDER MATTERS, and it used to be backwards. The `caos` remote only needs the
# server's URL, not a tunnel that carries data, so it is added FIRST -- before
# dumbpipe is even started. A resolver or a first tool call that runs while the
# tunnel is still coming up then finds the remote and waits the tunnel out
# (`ensure_server_reachable` retries a dead port); what it CANNOT retry away is
# a remote that is not there yet, and adding it last -- after a tunnel bring-up
# that can take twenty seconds -- is exactly what produced the intermittent
# "no `caos` git remote" that failed a session's whole first turn.

if [ -n "${CAOS_IROH_TICKET:-}" ]; then
    : "${server:=http://127.0.0.1:$port}"
fi

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

# UNSHALLOW THE CHECKOUT. caos pushes the WORKSPACE COMMIT to the server -- the
# resolver does it for a repo that defines `caos-tools/`, and every prompt does
# it as the conversation's base -- and that push packs the commit's whole
# reachable graph, HISTORY included. claude.ai/code clones shallow, so the
# history is not here, and the push dies "invalid commit object <HEAD>" against
# a server that does not already hold the repo. (caos' own sessions work only
# because that server was seeded with caos' history, making the push a thin
# delta; an arbitrary repo gets no such head start.) So fetch the rest ONCE,
# before the resolver or the first prompt tries to push. Non-fatal and quiet: a
# complete checkout, or a fetch that cannot reach the origin, just carries on --
# a big repo pays a one-time full-history fetch here rather than failing later.
if [ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]; then
    log "unshallowing the checkout so caos can push its history"
    git fetch --unshallow --quiet 2>/dev/null \
        || log "could not unshallow; a repo the server has not seen may fail to resolve"
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

# ---------------------------------------------------------------------------
# The tunnel, brought up now that the remote already points at its port
# ---------------------------------------------------------------------------

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
fi

exit 0
