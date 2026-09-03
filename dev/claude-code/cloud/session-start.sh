#!/bin/bash
# SessionStart hook for a caos cloud session.
#
# Wired from the repo's `.claude/settings.json`, which is what a cloud session
# reads ("Claude Code runs hooks from the repository"). Runs on EVERY session,
# cloud or local, including resumed ones, so it is idempotent and cheap when
# there is nothing to do.
#
# TWO JOBS, and only the first is permanent:
#
#   1. POINT THE CLIENT AT A SERVER. The client finds caos through the `caos`
#      git remote, and a freshly cloned repo has none. This is the piece that
#      still matters once caos is hosted: set CAOS_SERVER_URL on the
#      environment and this needs no other change.
#
#   2. BRING UP A STACK (experiment). Only while caos runs inside this VM.
#      Delete it when there is a server to point at.
set -uo pipefail

log() { printf 'caos session-start: %s\n' "$*" >&2; }

if [ -n "${CAOS_SKIP_SESSION_START:-}" ]; then
    log "CAOS_SKIP_SESSION_START set; doing nothing"
    exit 0
fi

# A local checkout already has its own remote and its own stack, and restarting
# either under a developer would be rude. Absence of the `caos` remote is the
# signal, not an Anthropic-specific marker: if the guess is ever wrong the worst
# case is that this declines to act.
if git remote get-url caos >/dev/null 2>&1; then
    log "this checkout already has a caos remote; leaving it alone"
    exit 0
fi

server="${CAOS_SERVER_URL:-http://localhost:9090}"
git remote add caos "$server" || true
log "caos remote -> $server"

# Anything other than the in-VM stack means the server is someone else's
# problem, which is the whole point of hosting it.
case "$server" in
    http://localhost:*|http://127.0.0.1:*) ;;
    *) log "hosted server; not starting a stack"; exit 0 ;;
esac

# ---------------------------------------------------------------------------
# The in-VM stack, while it still lives here
# ---------------------------------------------------------------------------

if curl -fsS --max-time 2 "$server/" >/dev/null 2>&1; then
    log "a stack is already answering on $server"
    exit 0
fi

export PATH=/nix/var/nix/profiles/default/bin:$PATH
if ! command -v nix >/dev/null 2>&1; then
    log "no nix; not a provisioned caos environment, skipping the stack"
    exit 0
fi

if ! docker info >/dev/null 2>&1; then
    log "starting dockerd"
    (dockerd >/tmp/dockerd.log 2>&1 &)
    for _ in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
fi
if ! docker info >/dev/null 2>&1; then
    log "dockerd did not come up; see /tmp/dockerd.log"
    exit 0
fi

# BACKGROUNDED: a cold `nix build` takes far longer than a session start should
# wait, and a blocking SessionStart hook leaves the user looking at nothing. The
# first caos tool call fails clearly rather than hanging, and the stack follows.
marker=/tmp/caos-stack-bringup
if [ -e "$marker" ]; then
    log "bring-up already in progress (see $marker)"
    exit 0
fi
: > "$marker"
log "building and starting the stack in the background; watch $marker"
(
    {
        echo "=== nix build ==="; nix build 2>&1
        echo "=== caosd up ==="; ./result/bin/caosd up 2>&1
        echo "=== done ==="
    } >>"$marker" 2>&1
) &

exit 0
