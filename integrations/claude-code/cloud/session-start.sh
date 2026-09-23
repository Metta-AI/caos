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
elif [ -z "$bootstrap_base" ]; then
    # NOT the same reason as the branch below, and saying so matters: the
    # checkout here may pin caos perfectly well. What is missing is a base to
    # fetch `caos-pin.sh` FROM, which only an environment snapshotted before
    # `--bootstrap-base` existed can be short of. Reporting that as "this
    # checkout pins no caos" sends the reader to the repository, which is the
    # one thing that is not wrong.
    log "no script base, so the repo's pin was never read (see the note above);"
    log "  using the client the environment was built with"
else
    log "this checkout pins no caos, so there is no commit to refresh from;"
    log "  using the client the environment was built with"
fi
step "client refresh done"

# ---------------------------------------------------------------------------
# DEV MODE: run the caos on the SERVER, not the one on GitHub
# ---------------------------------------------------------------------------
# `caosd up` publishes its checkout to `refs/caos/dev` on the stack it starts:
# one commit carrying the working tree and the x86_64 client built from it. With
# `CAOS_DEV=1` this session runs that instead of the repo's pin, so uncommitted
# work reaches a container with no GitHub push and no wait for CI.
#
# EVERY SESSION, not once at setup: that ref moves on every `caosd up`, so a
# value frozen into the snapshot would be the stale one of the two. This is the
# same reason the pin itself is re-read above.
#
# AFTER the refresh, deliberately. The install above is what puts
# `git-remote-caos` on disk, and without it `git` cannot speak `caos://` at all
# -- so the repo's ordinary pin is the bootstrap that makes this reachable, and
# it never has to move.
#
# Never fatal. A dev overlay that cannot happen leaves a session running the
# pinned client, which works; saying so beats failing to start.
# THE SAME PROBE setup.sh ran, so one session gives a controlled before/after
# across "Starting Claude Code" in a single container. Both go to STDOUT, which
# is the only stream a session can read back (this file's `log` is stderr, and
# setup.sh's output reaches only the env-manager log).
caos_probe() {
    for url in "$@"; do
        code="$(curl -sS -o /dev/null -m 8 -w '%{http_code}' "$url" 2>&1)" \
            || code="FAILED(${code##*: })"
        printf '%s=%s ' "${url#https://}" "$code"
    done
    printf '\n'
}
if [ -r /usr/local/share/caos/net-probe-setup ]; then
    printf 'caos net probe (setup phase): %s' "$(cat /usr/local/share/caos/net-probe-setup)"
    printf '\n'
else
    echo "caos net probe (setup phase): not recorded by this environment's setup."
fi
printf 'caos net probe (hook phase):  %s' \
    "$(caos_probe \
                  https://usw1-1.relay.n0.iroh.link./ \
                  https://use1-1.relay.n0.iroh.link./ \
                  https://euc1-1.relay.n0.iroh.link./ \
                  https://aps1-1.relay.n0.iroh.link./ \
                  https://use1-1.relay.iroh.network./ \
                  https://euw1-1.relay.iroh.network./ \
                  https://aps1-1.relay.iroh.network./ \
                  https://iroh.computer/ \
                  https://www.hetzner.com/ \
                  https://example.com/)"

# The same verbose trace setup.sh took, so the two can be diffed. Only the relay
# is traced: it is the one host whose answer differs by phase (503 vs 200), and
# the question is what in the path treats it differently.
caos_relay_trace() {
    echo "--- proxy env ---"
    proxies="$(env | grep -iE '^(https?_proxy|no_proxy)=' | cut -c1-120)"
    if [ -n "$proxies" ]; then printf '%s\n' "$proxies"; else echo "(no proxy variables set)"; fi
    echo "--- path, tls, alpn, status ---"
    # GREPPED, NOT TRUNCATED. An earlier version piped this through `head` and
    # cut both phases off before the response, so the 503 everything was being
    # argued about was never actually observed.
    curl -sS -m 10 -v -o /tmp/caos-relay-body -D /tmp/caos-relay-hdr         https://usw1-1.relay.n0.iroh.link./ 2>&1         | grep -E "Trying |Connected to |CONNECT tunnel|Establish HTTP proxy|ALPN: |subject:|issuer:|using HTTP|^< HTTP|error|refused|timed out"
    echo "--- response headers ---"
    cat /tmp/caos-relay-hdr 2>/dev/null
    echo "--- body (first 200 bytes) ---"
    head -c 200 /tmp/caos-relay-body 2>/dev/null; echo
}
echo "===== CAOS RELAY TRACE: SETUP PHASE ====="
if [ -r /usr/local/share/caos/relay-trace-setup ]; then
    cat /usr/local/share/caos/relay-trace-setup
else
    echo "(not recorded by this environment's setup)"
fi
echo "===== CAOS RELAY TRACE: HOOK PHASE ====="
caos_relay_trace 2>&1
echo "===== CAOS RELAY TRACE END ====="

if [ "${CAOS_DEV:-}" = 1 ] && [ "$have_repo" = 1 ] && [ -n "$server" ]; then
    step "dev mode: looking for refs/caos/dev on the server"
    # STDERR IS KEPT. An empty result was reported as "the server publishes no
    # refs/caos/dev" whatever had actually gone wrong, and the two causes want
    # opposite fixes: `git-remote-caos` off PATH (git cannot speak `caos://` at
    # all) or the endpoint unreachable. Discarding it cost a long detour in
    # which the same silence was argued both ways from a slow runtime.
    dev_err="$(mktemp)"
    dev_sha="$(git ls-remote "$server" refs/caos/dev 2>"$dev_err" | awk '{print $1}')"
    if [ -z "$dev_sha" ]; then
        log "CAOS_DEV=1 but no refs/caos/dev came back from the server."
        log "  git-remote-caos: $(command -v git-remote-caos || echo 'NOT ON PATH')"
        log "  ls-remote said: $(tr '\n' ' ' < "$dev_err" | cut -c1-200)"
        log "  If that is a timeout, check 'caosd up --iroh' is serving this ticket."
    elif ! git fetch --quiet --depth=1 --no-tags --no-write-fetch-head \
              -- "$server" "$dev_sha" 2>"$dev_err"; then
        log "could not fetch $dev_sha; staying on the pinned client"
        log "  fetch said: $(tr '\n' ' ' < "$dev_err" | cut -c1-200)"
        dev_sha=""
    fi
    rm -f "$dev_err"
fi
if [ -n "${dev_sha:-}" ]; then
    # THE BINARIES, over the ones install.sh just placed. Written beside and
    # RENAMED rather than truncated in place: these are executables, and
    # overwriting a mapped file is ETXTBSY rather than an update.
    #
    # Into `lib/caos`, which is where the CLIENT looks -- `bin/caos` is a shell
    # wrapper that execs it, and `ensure_helper_on_path` puts `lib/caos` on PATH
    # before shelling out to git. A helper written only into `bin` is invisible
    # to caos' own git.
    overlaid=""
    for name in caos git-remote-caos; do
        dest="/usr/local/lib/caos/$name"
        if git cat-file blob "$dev_sha:dev-bin/$name" > "$dest.dev" 2>/dev/null; then
            chmod 0755 "$dest.dev" && mv -f "$dest.dev" "$dest" && overlaid="$overlaid $name"
        else
            rm -f "$dest.dev"
        fi
    done
    if [ -n "$overlaid" ]; then
        # AND SAY SO IN THE VERSION, because otherwise nothing does. `bin/caos`
        # is a wrapper that exports a `CAOS_REV` baked in at install time, and
        # the overlay above replaces only the binary it execs -- so `caos
        # --version` and `caos_status` both go on reporting the PINNED build
        # while a different one runs. Measured in a live session: diagnostics
        # read `client CAOS_REV: build-3bf5b67cd5a7` with the dev client in
        # place, which is the exact reading that would send someone to debug
        # why dev mode had not taken.
        if [ -w /usr/local/bin/caos ]; then
            sed -i "s|CAOS_REV:-[^}]*}|CAOS_REV:-dev-${dev_sha:0:12}}|" /usr/local/bin/caos \
                || log "could not stamp the wrapper; caos --version will name the pinned build"
        fi
        step "dev client installed:$overlaid (caos --version now says dev-${dev_sha:0:12})"
    else
        log "refs/caos/dev carries no dev-bin/; leaving the pinned client in place"
    fi

    # THE TOOLS, from the same commit. `std/` is compiled by caos itself from
    # the std tree, so it is reached by a locator rather than installed -- and
    # `git+caos://…` is an ordinary git fetch through `git-remote-caos`, which
    # is why this needs no new transport and no code change (git-locator takes
    # any `git+<scheme>`).
    #
    # BOTH FILES, or neither works: `std/flake-input-loader` refuses a tree
    # whose expression and `flake.lock` name different revisions, and it is
    # `flake.lock`'s URL that decides which locators it even checks.
    expr_file="$PWD/.caos-expr"
    lock_file="$PWD/flake.lock"
    if [ -w "$expr_file" ] && [ -r "$lock_file" ]; then
        # THE TICKET HAS TO REACH THE CONVERSATION, so nothing here may hide
        # these edits from git. `--skip-worktree` was tried, to stop an agent
        # committing a `caos://` URL that is also a credential, and it made dev
        # mode a no-op: caos ingests the checkout through `hash_dir`, which
        # copies the REAL index for its stat cache and so inherits that bit,
        # and `git add -u <dir>` on a skip-worktree path exits 0 and silently
        # keeps HEAD's blob. Both files reverted together, so the loader's
        # rev-drift check saw two consistent files and passed, and every
        # conversation evaluated the committed pin while the hook logged
        # success. The ticket goes to the user's own caosd either way; what is
        # left is an agent committing it upstream, and that is a push-time
        # concern, not a reason to lie to git about what is on disk.
        #
        # Every `:@@=` locator repointed at this server at this commit, keeping
        # each one's own `dir=`: the expression names two (the loader's image
        # and the tree it splices) and they differ only by that.
        sed -i "s|:@@=[^ ?]*?rev=[0-9a-f]*\&dir=\([^ ]*\)|:@@=git+$server?rev=$dev_sha\&dir=\1|g" \
            "$expr_file"

        # The lock's `locked` section for the caos input, found through
        # `nodes.<root>.inputs.<name>` as caos-pin.sh and the loader both do --
        # the node key is not the input name once an input has been renamed.
        # `narHash` is DROPPED rather than recomputed: an absent hash is honest,
        # where a stale one would be a lie nix would later reject.
        if tmp_lock="$(jq --arg url "$server" --arg rev "$dev_sha" '
                (.root // "root") as $r
                | (.nodes[$r].inputs.caos // empty) as $k
                | if ($k|type) == "string"
                  then .nodes[$k].locked = {type:"git", url:$url, rev:$rev}
                  else . end' "$lock_file" 2>/dev/null)"; then
            printf '%s\n' "$tmp_lock" > "$lock_file"
            dev_tools_ok=1
            step "dev tools: $caos_std_path/ now resolves from the server at ${dev_sha:0:12}"
        else
            log "could not rewrite flake.lock; the loader will refuse the drift it now sees"
        fi
    else
        log "no writable .caos-expr / flake.lock here; the tools stay on the repo's pin"
    fi
fi

# ---------------------------------------------------------------------------
# WHICH CAOS THIS SESSION IS -- on STDOUT, the only stream that reaches anyone
# ---------------------------------------------------------------------------
# A SessionStart hook contributes its STDOUT to the session. Its stderr is
# captured as a `system/hook_response` event, which is classed as NON-TRANSCRIPT
# and dropped -- the run-log API says so in as many words, and reading these
# lines at all meant paging the raw events endpoint three cursors back.
#
# Every line this file logs is stderr (`log()` redirects, `step()` calls `log`).
# So the dev-mode trace added to make "did dev mode take?" a FACT rather than an
# inference was written, every session, into the one stream the session discards
# -- and then cost exactly the debugging session it exists to prevent: a run
# whose hook had said `dev client installed ... dev-0ac792e540a0` read, from
# everywhere a human or a model can see, as a change that had not taken.
#
# ONE LINE on success, because this lands in every session's context and the
# detail belongs on stderr. The failure case gets two, and names which half
# failed: "asked for and did not happen" is the reading that sends someone to
# debug their own code instead of their environment.
if [ "${CAOS_DEV:-}" != 1 ]; then
    echo "caos dev mode: off -- this session runs the caos its repo pins."
elif [ -n "${overlaid:-}" ] && [ -n "${dev_tools_ok:-}" ]; then
    echo "caos dev mode: ON -- client and tools from refs/caos/dev at ${dev_sha:0:12}."
else
    echo "caos dev mode: ASKED FOR BUT NOT ACTIVE -- this session runs the PINNED caos."
    echo "  refs/caos/dev: ${dev_sha:-not published}; client overlay:${overlaid:- none};" \
         "tools rewrite: ${dev_tools_ok:+done}${dev_tools_ok:-NOT DONE}."
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
        # ONLY REACHABLE ON AN OLDER SNAPSHOT, and kept for exactly that. An
        # environment built by the current setup.sh always has a pin (it fails
        # without one), so `$caos_std_path` is always set above and this branch
        # is not taken. Where it IS taken, the snapshot's configuration names
        # the same `:@@=` form this builds, so the warm and the serve still
        # agree on a cache key -- which is the only thing that matters here,
        # since the registry cache is keyed by the `--llm-step` STRING.
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
