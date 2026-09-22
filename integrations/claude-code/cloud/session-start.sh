#!/bin/bash
# Installed by the cloud setup script as /usr/local/bin/caos-cloud-session-start
# and called from the user-level SessionStart hook. Runs at the start of every
# session, so it is idempotent and quiet when there is nothing to do.
#
# One job that must happen per session, because it does not survive the
# environment snapshot: point this checkout's `caos` remote at the server.
# That is what makes the whole arrangement repo-independent — the client finds
# caos through a `caos` git remote, an arbitrary clone has none, and this adds
# it from user-level configuration rather than from anything committed.
#
# ORDER STILL MATTERS, because a session's first tool call races this hook. The
# remote is what that call cannot survive the absence of, and it costs a
# `git remote add`, so it goes FIRST; the slow steps that need nothing from it —
# refreshing the client binary, unshallowing the checkout — go last, where they
# cannot delay it. Put a slow step first (as earlier versions did) and the
# model's opening `caos_status` reliably reported "no caos remote".
set -uo pipefail

log() { printf 'caos: %s\n' "$*" >&2; }

# EVERY LINE IS STAMPED with seconds since this hook started, because the gap
# this hook sits in -- Claude Code launching to the session's first turn -- was
# measured at 65s and could not be attributed to any step inside it. The hook's
# own steps, Claude Code's startup and its MCP connect all overlap in that
# window, so the only way to tell them apart is for each side to say when it ran.
# This is that side; `caos::timing` is the other.
hook_started="$(date +%s)"
step() { log "[+$(($(date +%s) - hook_started))s] $*"; }

# WHERE THE SIBLING SCRIPTS COME FROM -- a branch, normally -- and the ONLY
# durable argument this hook takes. Which caos to install is not passed in: it
# is read out of the checkout below, on every session, because that is the only
# copy that cannot be stale.
bootstrap_base=""
enable_bash=""
base=""
caos_std_path=""
legacy_base=""
for arg in "$@"; do
    case "$arg" in
        --bootstrap-base=*) bootstrap_base="${arg#--bootstrap-base=}" ;;
        --enable-bash) enable_bash=yes ;;
        # An OLDER snapshot's bootstrap passes `--base=<the pin at setup time>`
        # and `--caos-std-path=`. Both are refused rather than honoured: that
        # `--base` is a frozen pin masquerading as the script base, and using
        # it would fetch caos-pin.sh from whatever the repo pinned weeks ago.
        #
        # SAID LOUDLY, not aliased. The fix for an argument that moved is to
        # name it, the way `CAOS_IROH_TICKET` is named above -- an environment
        # that keeps answering to both spellings is how one ends up setting the
        # one nothing reads.
        --base=*) legacy_base="${arg#--base=}" ;;
        --caos-std-path=*) : ;;
    esac
done

# ONE NAME, because there is one thing to name: a ticket IS a server URL
# (design/iroh-transport.md), so `CAOS_IROH_TICKET` would be a second spelling
# of `CAOS_SERVER_URL` — and two names for one value is how an environment ends
# up setting the one nothing reads.
#
# It was briefly accepted as an alias, for environments configured against the
# old tunnel, and that is the mistake this note exists to not repeat: the fix
# for a variable that moved is to SAY SO, loudly, in the place the session can
# see, not to keep answering to both.
server="${CAOS_SERVER_URL:-}"
if [ -z "$server" ] && [ -n "${CAOS_IROH_TICKET:-}" ]; then
    log "CAOS_IROH_TICKET is set, and nothing reads it any more."
    log "Set CAOS_SERVER_URL to this server's ticket instead ('caosd ticket' prints it)."
fi

# Which environment is this? Printed first, every session, so that a stale
# snapshot announces itself instead of being mistaken for a broken fix.
if [ -r /usr/local/share/caos/setup-stamp ]; then
    while IFS= read -r line; do log "env $line"; done \
        < /usr/local/share/caos/setup-stamp
else
    log "no setup stamp; this environment predates it"
fi

# An environment snapshotted before the bootstrap stopped baking the pin. Its
# `--base` is that frozen pin, not a script base, so nothing here uses it — and
# with no `--bootstrap-base` this hook cannot read the repo's CURRENT pin,
# which means no client refresh. The session still runs on the client the
# snapshot holds; say why, once, where the person reading the log can act on it.
if [ -z "$bootstrap_base" ] && [ -n "$legacy_base" ]; then
    log "this environment's bootstrap passes --base=${legacy_base##*/}, which is a"
    log "  frozen pin rather than a script base. Nothing reads it any more."
    log "  REBUILD THE ENVIRONMENT (re-run its setup script) to get the pin"
    log "  re-read per session; until then the snapshot's client is used."
fi

# A ticket IS a server URL now (design/iroh-transport.md), so there is nothing
# to bring up and nothing to point at a local port: `caos://…` is handled by the
# client itself and by `git-remote-caos`, which the installer puts beside it.
# What used to be here — a dumbpipe connector on :19090, a liveness poll, a
# pkill for the one holding the port, and `setsid` so Claude Code's teardown of
# the hook's process group would not take it with it — is all gone with the
# process it babysat.
#
# SAY WHICH SERVER, and say it by prefix: a `caos://` URL ends in the token that
# authorizes driving that server, and these lines are read back by whoever is
# debugging a session.
#
# The truncation is only right for a ticket: `${x%.*}` on `http://10.0.0.5:9090`
# would cut the address instead.
shown="$server"
case "$server" in
    caos://*) shown="${server%.*}…" ;;
esac
if [ -n "$server" ]; then
    log "server $shown"
fi

# ---------------------------------------------------------------------------
# The repo -- found FIRST, because the remote needs it
# ---------------------------------------------------------------------------
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
# Say that a registry is coming, BEFORE doing anything that takes time
# ---------------------------------------------------------------------------
# Claude Code spawns the caos tool server in PARALLEL with this hook, not after
# it, and that server's first `tools/list` lands within a second -- while this
# hook is still adding the remote. Without a claim already on disk, the server
# sees nobody working, resolves the tools itself, and the warm below duplicates
# it: measured at 8.5s and 8.2s side by side, colliding on a push of the same
# object. `mcp serve` waits for this file instead (`ensure_resolved`).
#
# So it is written HERE, first, rather than by the warm that eventually fills
# the cache -- a claim taken when the warm starts is taken far too late to be
# seen. The deadline it carries bounds a hook that dies without its trap.
#
# Removed on EVERY exit, including the paths below that decide not to warm at
# all: a marker left behind makes the next server in this checkout wait out its
# whole budget for a hook that is long gone.
if [ "$have_repo" = 1 ] && marker_dir="$(git rev-parse --absolute-git-dir 2>/dev/null)"; then
    warm_marker="$marker_dir/caos-cc-warming"
    printf '%s\n' "$(( $(date +%s) + 180 ))" > "$warm_marker"
    trap 'rm -f "$warm_marker"' EXIT
fi

# ---------------------------------------------------------------------------
# The remote, FIRST
# ---------------------------------------------------------------------------
# The one thing a session's first tool call cannot survive is a missing `caos`
# remote: the resolver and a first `caos_status` retry a server that is briefly
# unreachable, but CANNOT retry away a remote that is not there yet. And it
# needs only a URL and a repo -- neither the client refresh nor
# the unshallow -- so it is added within a second of the hook starting.
#
# An existing remote is left alone: a checkout that already names a caos server
# has been set up deliberately, and repointing it would silently move someone's
# work to a different stack.
if [ -n "$server" ] && [ "$have_repo" = 1 ]; then
    if current="$(git remote get-url caos 2>/dev/null)"; then
        if [ "$current" != "$server" ]; then
            log "caos remote already set; leaving it (wanted $shown)"
        fi
    else
        git remote add caos "$server" && step "caos remote -> $shown"
    fi
elif [ -z "$server" ]; then
    log "no CAOS_SERVER_URL; leaving the remote alone"
else
    log "$PWD is not a git repository; nothing to point at $shown"
fi


# ---------------------------------------------------------------------------
# The client, refreshed -- AFTER the remote (slow: may download)
# ---------------------------------------------------------------------------
# The setup script installed one, into a filesystem that is then FROZEN. A push
# does not reach an existing environment, so a client left where the setup put
# it is as old as the environment is, no matter how many sessions start.
#
# Re-running the installer costs one `git ls-remote` when nothing moved, because
# it stops as soon as it finds the build already installed; when the build DID
# move it downloads the new binary, which is why it runs here and not ahead of
# the remote. Not fatal on failure: a working client that is out of
# date beats no session at all.
# `--user-config` re-writes the configuration in the SAME pass, because it pins
# the commit the client was built from: a refreshed client left with the old
# configuration would drive a step from a different tree than itself, the one
# pairing that cannot go quiet. Both are a no-op when nothing moved.
# RE-READ the repo's pin first, because the snapshot froze the last one.
#
# The environment's snapshot carries whatever caos the setup script resolved,
# and a push to the client repo does not reach it -- so a repo that has since
# re-pinned would keep getting the OLD client and the OLD tools, agreeing with
# each other and with nothing the repo says. That is the half-update the rest of
# this file is arranged to avoid, and it is why the pin is read again here
# rather than trusted from the bootstrap.
#
# Cheap when nothing moved: one jq over flake.lock, and the install below then
# stops at a single `ls-remote`.
#
# THE PIN IS THE ONLY SOURCE OF A BASE FOR THE INSTALL, and `$base` starts
# EMPTY to make that structural rather than a convention. `$bootstrap_base`
# names a branch -- it is where these scripts come from -- and install.sh
# refuses a branch, because the step resolves through the pinned commit and a
# client from a moving head would be a client from another tree. So a checkout
# that pins no caos does not get a refresh at all; it keeps the client the
# snapshot has, which is a session that works rather than one installed from
# the wrong tree.
if [ "$have_repo" = 1 ] && [ -n "$bootstrap_base" ]; then
    # Cleared before the eval, and read back with `:-` after it, so a
    # caos-pin.sh that somehow succeeds while printing less than it promises
    # cannot take the hook out on an unset variable under `set -u`. The whole
    # point of this block is to be optional; it must not become the thing that
    # stops a session starting.
    caos_pin_base=""; caos_pin_std_path=""; caos_pin_repo=""; caos_pin_rev=""
    if pin="$(curl -fsSL "$bootstrap_base/integrations/claude-code/cloud/caos-pin.sh" \
              2>/dev/null | bash -s -- "$PWD" 2>/dev/null)"; then
        eval "$pin" || true
    fi
    if [ -n "${caos_pin_base:-}" ] && [ -n "${caos_pin_std_path:-}" ]; then
        # Printed EVERY session, not only when it changes. There is nothing to
        # compare it against -- the bootstrap no longer carries a previous pin,
        # which is the point -- and this one line is what says which caos the
        # session is about to be, beside the stamp saying which environment it
        # is. Together they are how a stale snapshot tells itself apart from a
        # fix that did not work.
        step "the repo pins caos ${caos_pin_repo:-?} at ${caos_pin_rev:0:12}, std at $caos_pin_std_path"
        base="$caos_pin_base"
        caos_std_path="$caos_pin_std_path"
    fi
fi

if [ -n "$base" ] && [ -n "$caos_std_path" ]; then
    step "refreshing the client"
    if ! curl -fsSL "$base/integrations/claude-code/cloud/install.sh" \
         | bash -s -- --no-repo-files --user-config --base="$base" \
               ${enable_bash:+--enable-bash} \
               --caos-std-path="$caos_std_path"; then
        log "could not refresh the client; carrying on with the installed one"
    fi
else
    log "this checkout pins no caos, so there is no commit to refresh from;"
    log "  using the client the environment was built with"
fi
step "client refresh done"

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
# prompt tries to push -- but AFTER the remote, which the first tool
# call needs sooner and this must not delay. Non-fatal and quiet: a complete
# checkout, or a fetch that cannot reach the origin, just carries on -- a big
# repo pays a one-time full-history fetch here rather than failing later.
if [ "$have_repo" = 1 ] \
    && [ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]; then
    step "unshallowing the checkout so caos can push its history"
    git fetch --unshallow --quiet 2>/dev/null \
        || log "could not unshallow; a repo the server has not seen may fail to resolve"
fi
step "unshallow done"

# ---------------------------------------------------------------------------
# Warm the tool registry -- LAST, after the checkout is complete enough to push
# ---------------------------------------------------------------------------
# `mcp serve` cannot resolve the tools before it must answer the client's first
# `tools/list` -- the resolution may build an image -- so a client that reads
# that list exactly once at startup (the mounted Claude Code a cloud
# environment uses) is left with no caos tools however fast the resolve then
# finishes. Resolve them HERE instead, while this hook still blocks the session
# from starting, and leave them in the cache `mcp serve` reads when it launches.
# Then the first turn has the tools rather than racing a background resolve it
# cannot see and cannot wait out.
#
# The step is pinned exactly as install.sh --user-config pins it: to the commit THIS client
# was built from (the build record), so the warm resolves the same tree the
# serve will. Bounded and non-fatal -- a warm that cannot finish just leaves the
# background path in place, which is where we were before this ran.
if [ "$have_repo" = 1 ] && [ -n "$server" ] && command -v caos >/dev/null 2>&1; then
    locator=""
    if [ -n "$caos_std_path" ]; then
        # A repo-pinned step: the SAME path the config names, so the warm fills
        # the cache the serve then reads. Naming it any other way would resolve
        # a different tree and leave the first turn with no tools while a
        # perfectly good registry sat in the cache under another key.
        locator="--llm-step:@=$caos_std_path/llm-step"
    else
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
        fi
    fi
    if [ -n "$locator" ]; then
        step "warming the caos tool registry for the first turn"
        # Its output goes to a FILE, not the hook's own stdout/stderr, and this
        # is not tidiness: Claude Code holds the session at "starting" until this
        # hook's output stream reaches EOF, so a warm that inherited that stream
        # and left a child (a `git` the resolve forked) writing to it would keep
        # the WHOLE SESSION from starting long after the warm itself returned.
        # The redirect closes the hook's stream the moment the foreground `caos`
        # exits.
        #
        # Bounded, but not tightly: the resolve talks to the caos server over the
        # `caos://` transport and does real work there (it may run the step to
        # list its tools), measured around 110s in a cloud session -- so a
        # cap short enough to be "tight" would time out every warm and cache
        # nothing, which is the whole point of it. 150s covers the observed
        # resolve with room; `-k` turns SIGTERM into SIGKILL 10s later so a
        # resolve that ignores the term (a blocked network read) cannot hold the
        # hook past the cap. A warm that still does not finish leaves the
        # background resolve in place, which is where this was before it existed.
        CLAUDE_PROJECT_DIR="$PWD" timeout -k 10 150 caos mcp warm "$locator" \
            >/tmp/caos-warm.log 2>&1 \
            || log "could not warm the tools in time; mcp serve will resolve in the background"
        while IFS= read -r line; do log "warm: $line"; done < /tmp/caos-warm.log
        step "warm done"
    else
        log "nothing names the step (no repo pin and no build record);" \
            "leaving the tools to mcp serve's background resolve"
    fi
fi

step "hook done"

exit 0
