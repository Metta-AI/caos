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
#
# ORDER IS THE WHOLE GAME HERE, because a session's first tool call races this
# hook. The two things that call needs -- the `caos` remote and a live tunnel --
# are quick, so they go FIRST; the two slow steps that need neither -- refreshing
# the client binary and unshallowing the checkout -- go last, where they cannot
# delay them. Put a slow step first (as earlier versions did, either way round)
# and the model's opening `caos_status` reliably reported "no caos remote" or
# "connection refused" for a remote/tunnel merely queued behind it.
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
# The server URL and the repo -- computed FIRST, because the remote needs them
# ---------------------------------------------------------------------------
# A ticket names an iroh NODE, and the node id is the identity: it comes from
# the listener's IROH_SECRET, so a ticket keeps working across restarts of the
# listener even though the address embedded in it goes stale -- which is why one
# ticket lives in the environment indefinitely. The remote points at the local
# port the tunnel binds.
if [ -n "${CAOS_IROH_TICKET:-}" ]; then
    : "${server:=http://127.0.0.1:$port}"
fi

# The repo is named, not assumed from cwd. A hook's working directory is not
# contractually the project -- in a cloud session the checkout is at
# /home/user/repo while $HOME resolves to /root -- and a wrong cwd here does not
# error, it silently adds the remote to some other repository or to none, and
# the failure only shows up much later as a client that cannot find a server.
if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -d "$CLAUDE_PROJECT_DIR" ]; then
    cd "$CLAUDE_PROJECT_DIR" || true
fi
have_repo=0
if git rev-parse --git-dir >/dev/null 2>&1; then
    have_repo=1
fi

# ---------------------------------------------------------------------------
# The remote, FIRST
# ---------------------------------------------------------------------------
# The one thing a session's first tool call cannot survive is a missing `caos`
# remote: the resolver and a first `caos_status` retry a dead PORT (the tunnel
# still coming up) but CANNOT retry away a remote that is not there yet. And it
# needs only a URL and a repo -- neither the tunnel below, the client refresh nor
# the unshallow -- so it is added within a second of the hook starting.
#
# An existing remote is left alone: a checkout that already names a caos server
# has been set up deliberately, and repointing it would silently move someone's
# work to a different stack.
if [ -n "$server" ] && [ "$have_repo" = 1 ]; then
    if current="$(git remote get-url caos 2>/dev/null)"; then
        if [ "$current" != "$server" ]; then
            log "caos remote already set to $current; leaving it (wanted $server)"
        fi
    else
        git remote add caos "$server" && log "caos remote -> $server"
    fi
elif [ -z "$server" ]; then
    log "no CAOS_SERVER_URL and no CAOS_IROH_TICKET; leaving the remote alone"
else
    log "$PWD is not a git repository; nothing to point at $server"
fi

# ---------------------------------------------------------------------------
# The tunnel, SECOND -- before the two slow steps, because the remote points at
# its port and every server round trip needs it live
# ---------------------------------------------------------------------------
# It does NOT depend on the client refresh (dumbpipe is installed at env setup)
# nor on the unshallow (which fetches from origin, not through here), and both of
# those run for tens of seconds to minutes -- so the tunnel comes up first, or a
# first tool call reaches the remote and gets "connection refused" on a port
# nothing is bound to yet.
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

# ---------------------------------------------------------------------------
# The client, refreshed -- AFTER the remote and tunnel (slow: may download)
# ---------------------------------------------------------------------------
# The setup script installed one, into a filesystem that is then FROZEN. A push
# does not reach an existing environment, so a client left where the setup put
# it is as old as the environment is, no matter how many sessions start.
#
# Re-running the installer costs one `git ls-remote` when nothing moved, because
# it stops as soon as it finds the build already installed; when the build DID
# move it downloads the new binary, which is why it runs here and not ahead of
# the remote and tunnel. Not fatal on failure: a working client that is out of
# date beats no session at all.
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

# ---------------------------------------------------------------------------
# Unshallow the checkout -- LAST, because it is the slowest and gates only the
# first PUSH, which happens later than the first tool call
# ---------------------------------------------------------------------------
# caos pushes the WORKSPACE COMMIT to the server -- the resolver does it for a
# repo that defines `caos-tools/`, and every prompt does it as the conversation's
# base -- and that push packs the commit's whole reachable graph, HISTORY
# included. claude.ai/code clones shallow, so the history is not here, and the
# push dies "invalid commit object <HEAD>" against a server that does not already
# hold the repo. (caos' own sessions work only because that server was seeded
# with caos' history, making the push a thin delta; an arbitrary repo gets no
# such head start.) So fetch the rest ONCE, before the resolver or the first
# prompt tries to push -- but AFTER the remote and tunnel, which the first tool
# call needs sooner and this must not delay. Non-fatal and quiet: a complete
# checkout, or a fetch that cannot reach the origin, just carries on -- a big
# repo pays a one-time full-history fetch here rather than failing later.
if [ "$have_repo" = 1 ] \
    && [ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]; then
    log "unshallowing the checkout so caos can push its history"
    git fetch --unshallow --quiet 2>/dev/null \
        || log "could not unshallow; a repo the server has not seen may fail to resolve"
fi

# ---------------------------------------------------------------------------
# Warm the tool registry -- LAST, after the checkout is complete enough to push
# ---------------------------------------------------------------------------
# `cc serve` cannot resolve the tools before it must answer the client's first
# `tools/list` -- the resolution may build an image -- so a client that reads
# that list exactly once at startup (the mounted Claude Code a cloud
# environment uses) is left with no caos tools however fast the resolve then
# finishes. Resolve them HERE instead, while this hook still blocks the session
# from starting, and leave them in the cache `cc serve` reads when it launches.
# Then the first turn has the tools rather than racing a background resolve it
# cannot see and cannot wait out.
#
# The step is pinned exactly as configure.sh pins it: to the commit THIS client
# was built from (the build record), so the warm resolves the same tree the
# serve will. Bounded and non-fatal -- a warm that cannot finish just leaves the
# background path in place, which is where we were before this ran.
if [ "$have_repo" = 1 ] && [ -n "$server" ] && command -v caos >/dev/null 2>&1; then
    record=/usr/local/share/caos/build
    step_repo=""
    step_commit=""
    if [ -r "$record" ]; then
        while IFS='=' read -r key value; do
            case "$key" in
                repo) step_repo="$value" ;;
                commit) step_commit="$value" ;;
            esac
        done < "$record"
    fi
    if [ -n "$step_repo" ] && [ -n "$step_commit" ]; then
        locator="--llm-step:@@=github:$step_repo?rev=$step_commit&dir=std/llm-step"
        log "warming the caos tool registry for the first turn"
        # Bounded TIGHTLY, and its output sent to a FILE, not the hook's own
        # stdout/stderr. Both matter for the same reason: Claude Code holds the
        # session at "starting" until this hook's output stream reaches EOF, so a
        # warm that inherited that stream and left a child (a `git` the resolve
        # forked) writing to it would keep the WHOLE SESSION from starting long
        # after the warm itself returned. The redirect closes the hook's stream
        # the moment the foreground `caos` exits; the 60s cap keeps a cold image
        # build -- which no blocking step should wait out -- from delaying the
        # session either. A warm that does not finish just leaves the background
        # resolve in place, which is where this was before.
        CLAUDE_PROJECT_DIR="$PWD" timeout 60 caos cc warm "$locator" \
            >/tmp/caos-warm.log 2>&1 \
            || log "could not warm the tools in time; cc serve will resolve in the background"
        while IFS= read -r line; do log "warm: $line"; done < /tmp/caos-warm.log
    else
        log "no build record; leaving the tools to cc serve's background resolve"
    fi
fi

exit 0
