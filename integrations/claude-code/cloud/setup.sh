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
#   B=https://raw.githubusercontent.com/Metta-AI/caos/main
#   curl -fsSL "$B/integrations/claude-code/cloud/setup.sh" | bash -s -- --base="$B"
#
# `--base` says where the BOOTSTRAP SCRIPTS come from -- this file,
# caos-pin.sh, session-start.sh -- and nothing else. A script piped into bash
# cannot see its own URL (no $0, no path, no referrer), so it has to be told
# once, and once is all it is told. It may name a branch, and usually should:
# the settings form is then never edited again.
#
# IT DOES NOT SAY WHICH CAOS. That comes from the REPOSITORY the environment is
# pointed at -- a client repo, whose flake.lock pins a commit and whose root
# .caos-expr mounts that commit's std. This script reads the pin and hands the
# COMMIT to install.sh, which refuses anything else. A repository with no pin
# is a misconfigured environment and fails below, rather than being installed
# from this branch: the step resolves through the pinned commit, so a client
# from a moving head would be a client from another tree than its own tools.
#
# Swap `main` for a branch or a commit to test a change to the SCRIPTS.
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
base="$RAW/Metta-AI/caos/main"
bootstrap_base=""
enable_bash=""
for arg in "$@"; do
    case "$arg" in
        --base=*) base="${arg#--base=}"; base="${base%/}" ;;
        # Passed straight through to install.sh, here and in every later session
        # (baked into the session-start bootstrap below): allow Bash/Read/Grep in
        # the deny list this env writes, so a session can inspect the container.
        --enable-bash) enable_bash=yes ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done
# The base this script was FETCHED from, kept before the repo is allowed to
# replace it: the sibling scripts (caos-pin.sh, session-start.sh) have to come
# from the same place as this file, or a checkout could point the bootstrap at a
# tree that never contained one.
bootstrap_base="$base"

# The repo and the ref come back out of the base, which is why there is only
# one thing to state. Shape-checked only: install.sh takes the same --base and
# does the real parse, so repeating that here would be a second rule to keep in
# step. The check is worth its four lines anyway -- a typo caught now names the
# typo, where the same typo caught later is a 404 on a URL nobody typed.
case "$base" in
    "$RAW"/*/*/*) ;;
    *)
        echo "FATAL: --base must look like" >&2
        echo "  $RAW/<owner>/<repo>/<ref>" >&2
        echo "  got: $base" >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# The repo's own pin, if it has one
# ---------------------------------------------------------------------------
# A caos-client repo DECLARES which caos it uses, in flake.lock, and where it
# mounts caos' std, in its root `.caos-expr`. When the checkout says both, it
# outranks `--base`: the client, the tools and the tree the session evaluates
# then all come from the commit the repo pins, and the two lines in the settings
# form stop being a version at all -- they are only where the bootstrap scripts
# come from.
#
# THE CHECKOUT IS ALREADY HERE. Measured from a session's env_manager_log:
# "Cloned from seed bundle" precedes "Running setup script", so this can read
# the repo rather than defer to the first session hook -- which matters because
# work done after the snapshot is paid by EVERY session, and this is the whole
# install.
caos_std_path=""
repo_dir=""
# `$CLAUDE_PROJECT_DIR` is almost certainly NOT set here -- it is a Claude Code
# hook variable and Claude Code has not started yet -- so the glob is what
# actually finds the checkout. It is tried first anyway, for the day that
# changes, and costs one test.
#
# `-e .git`, not `-d`: a worktree's `.git` is a FILE, and refusing one would
# send this down the fallback for a checkout that is perfectly good.
#
# An unmatched glob stays literal in bash, which these tests then reject, so
# there is no case where the literal is mistaken for a directory.
for candidate in "${CLAUDE_PROJECT_DIR:-}" /home/user/*/ /home/user; do
    [ -n "$candidate" ] || continue
    candidate="${candidate%/}"
    [ -e "$candidate/.git" ] || continue
    [ -r "$candidate/flake.lock" ] || continue
    repo_dir="$candidate"
    break
done
if [ -n "$repo_dir" ]; then
    echo "reading the caos pin from $repo_dir" >&2
    # Cleared first and read back with `:-`, so a reader that exits 0 while
    # printing less than it promises cannot leave `set -u` to abort the whole
    # setup over a fallback that was meant to be optional.
    caos_pin_base=""; caos_pin_std_path=""; caos_pin_repo=""; caos_pin_rev=""
    if pin="$(curl -fsSL "$bootstrap_base/integrations/claude-code/cloud/caos-pin.sh" \
              | bash -s -- "$repo_dir")"; then
        eval "$pin" || true
    fi
    if [ -n "${caos_pin_base:-}" ] && [ -n "${caos_pin_std_path:-}" ]; then
        base="$caos_pin_base"
        caos_std_path="$caos_pin_std_path"
        echo "this repo pins caos ${caos_pin_repo:-?} at ${caos_pin_rev:-?}" >&2
        echo "  and mounts its std at $caos_std_path" >&2
    fi
fi

# NO PIN, NO INSTALL -- and this is where the old fallback to `--base` lived.
#
# `--base` names a BRANCH (that is its job: it is where these bootstrap scripts
# come from, and the two lines in the settings form should not need editing per
# caos commit). Handing that branch to install.sh is what the fallback did, and
# it is exactly the pairing the whole arrangement is arranged to prevent: the
# step a session runs resolves through the commit the REPO pins, so a client
# installed from a branch head is a client from a different tree than its own
# tools.
#
# A session repo that pins no caos is therefore not something to paper over
# with the nearest available version. It is a misconfigured environment, and
# saying so here -- before the snapshot, where the message is read by whoever
# is setting it up -- is worth more than a session that starts and then
# behaves oddly.
if [ -z "$caos_std_path" ]; then
    echo "FATAL: this environment's repository does not pin caos." >&2
    echo "  A caos session starts from a CLIENT repo, which is four things:" >&2
    echo "    flake.nix + flake.lock pinning a 'caos' input by revision," >&2
    echo "    a root .caos-expr mounting that input's std (--output-path)," >&2
    echo "    the AGENTS.md the agent is given, and" >&2
    echo "    .caos-secrets declaring what it may use." >&2
    echo "  Point this environment at one, or fork Metta-AI/caos-session." >&2
    if [ -n "$repo_dir" ]; then
        echo "  Read $repo_dir/flake.lock; caos-pin.sh's reason is above." >&2
    else
        echo "  No checkout with a flake.lock was found under /home/user." >&2
    fi
    exit 1
fi

# WHICH CLIENT: the commit the repo pins, which `$base` now holds -- never the
# branch this script was fetched from. `$bootstrap_base` keeps that, for the
# sibling scripts, and the two must not be confused: one is where the scripts
# come from, the other is which caos the session IS.
#
# CAOS_SERVER_URL is the one environment variable left, and could not be
# anything else: it is read at SESSION start, long after this has run and been
# snapshotted, so no argument here could carry it.
args="--no-repo-files --user-config --base=$base${enable_bash:+ --enable-bash}"
args="$args${caos_std_path:+ --caos-std-path=$caos_std_path}"
installer="$base/integrations/claude-code/cloud/install.sh"

# `--no-repo-files --user-config`: the client goes on PATH and its deny list,
# hooks and server declaration go USER-level (pinned to the commit just
# installed), leaving the checkout exactly as it was found. install.sh does both
# in one pass -- there is no separate configure step -- because the config is
# never wanted without an install and needs the very commit the install resolved.
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

# The git remote helper arrives with the client, from the same release and into
# the same directory, so there is nothing to install here — a `caos://` server
# needs both (`git` execs the helper by name; the client finds it beside
# itself).

# The per-session work: the git remote. What goes in the
# snapshot is a BOOTSTRAP that fetches the real script every session, not the
# script itself.
#
# Everything this file writes is frozen the moment the environment is
# snapshotted, and later sessions skip this file entirely, so a fix pushed to
# git does NOT reach an existing environment however many sessions are started.
# A new session is not a new environment.
#
# Two lines in a settings form, one of them naming a ref, is worth keeping
# stable. The scripts behind it are not. So the only durable state here is the
# BOOTSTRAP base, and every session re-reads what that ref says today.
#
# THE BOOTSTRAP BASE, NOT THE PIN, and the difference is the whole point of
# this block. Baking the pin here (which it used to) made the session scripts
# come from whatever commit the repo pinned at SETUP time, frozen -- so a fix
# to session-start.sh could not reach an existing environment until someone
# both re-pinned the repo AND rebuilt the environment, and the split this file
# documents ("--base says where the scripts come from") was not true of its own
# bootstrap.
#
# Nothing about the CLIENT rides here any more either. `base` and
# `caos_std_path` are not written: session-start.sh re-reads the pin from the
# checkout on every session, which it must do anyway to catch a repo that has
# re-pinned, so a copy frozen at setup time could only ever be the stale one of
# the two.
cat > /usr/local/bin/caos-cloud-session-start <<EOF
#!/bin/bash
bootstrap_base="$bootstrap_base"
enable_bash="$enable_bash"
EOF
cat >> /usr/local/bin/caos-cloud-session-start <<'BOOTSTRAP'
# Never fatal: a session that cannot reach GitHub should still start, with the
# reason on stderr, rather than be blocked by its own setup.
if ! script="$(curl -fsSL "$bootstrap_base/integrations/claude-code/cloud/session-start.sh")"; then
    echo "caos: could not fetch $bootstrap_base/integrations/claude-code/cloud/session-start.sh; skipping" >&2
    exit 0
fi
exec bash -c "$script" caos-cloud-session-start --bootstrap-base="$bootstrap_base" \
    ${enable_bash:+--enable-bash}
BOOTSTRAP
chmod 0755 /usr/local/bin/caos-cloud-session-start
bash -n /usr/local/bin/caos-cloud-session-start || {
    echo "FATAL: the session-start bootstrap does not parse" >&2
    exit 1
}

# The user-level configuration was written by the `--user-config` install above;
# session-start re-runs the same install each session to keep it pinned to the
# refreshed client. There is no separate configure step to run here.

# ---------------------------------------------------------------------------
# What can this phase actually reach?
# ---------------------------------------------------------------------------
# A claim that this phase cannot reach the caos server was made from a 30s
# `ls-remote` timeout and then argued rather than measured. This measures it,
# and the SAME probe runs in session-start.sh, so one session yields a
# controlled before/after across "Starting Claude Code" in one container.
#
# THE RELAY IS THE INTERESTING URL. A `caos://` ticket from a dev stack carries
# only private direct addresses (`10.200.0.3:11204 127.0.0.1:11204`), so no
# container can dial the endpoint directly and every connection goes through
# the relay the ticket names. GitHub is the control -- known to work here,
# since this script has already downloaded from it -- and example.com says
# whether any non-allowlisted host answers.
install -d /usr/local/share/caos
caos_probe() {
    for url in "$@"; do
        # -k: the bare-IP probes below would otherwise fail on certificate
        # mismatch, which is indistinguishable from the connection being
        # refused -- and telling those apart is the whole point. Certificate
        # identity is observed in the relay trace instead, which does verify.
        code="$(curl -sS -k -o /dev/null -m 8 -w '%{http_code}' "$url" 2>&1)" \
            || code="FAILED(${code##*: })"
        printf '%s=%s ' "${url#https://}" "$code"
    done
    printf '\n'
}
caos_probe \
           https://usw1-1.relay.n0.iroh.link./ \
           https://use1-1.relay.n0.iroh.link./ \
           https://euc1-1.relay.n0.iroh.link./ \
           https://aps1-1.relay.n0.iroh.link./ \
           https://use1-1.relay.iroh.network./ \
           https://euw1-1.relay.iroh.network./ \
           https://aps1-1.relay.iroh.network./ \
           https://5.78.69.43/ \
           https://116.203.71.221/ \
           https://iroh.computer/ \
           https://www.hetzner.com/ \
           https://example.com/ \
    > /usr/local/share/caos/net-probe-setup 2>&1

# WHY ONLY THE RELAY. Measured: from this phase the relay answers 503 while
# example.com answers 200, so this is not a blocked network and not a domain
# allowlist -- something in the path treats that host differently, or is not
# ready yet. The verbose trace says which: whether a proxy is in front (CONNECT,
# Via:), whose certificate is presented (a TLS-intercepting proxy shows its
# own), and whether the 503 carries the relay's body or a proxy's.
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
caos_relay_trace > /usr/local/share/caos/relay-trace-setup 2>&1

# ---------------------------------------------------------------------------
# When did this environment last get built?
# ---------------------------------------------------------------------------
# "Did the rebuild happen?" has to be a FACT, not an inference. A setup script
# runs once and is then frozen into a snapshot, and a session started afterwards
# looks identical whether the environment was rebuilt or not -- so a fix that
# was pushed but never picked up presents as a fix that did not work, and the
# debugging goes to the code instead of to the snapshot.
#
# The session hook prints this, so every session says which environment it is.
install -d /usr/local/share/caos
{
    echo "built:  $(date --iso-8601=seconds 2>/dev/null || date)"
    echo "base:   $base"
    # `--version` is not a flag: the client answers with its usage, whose first
    # line is `<prog> (<rev>)` -- carrying the `caos: ` prefix `main` puts on an
    # error. Stripped here, or the stamp reads `client: caos: caos (build-…)`.
    client="$(caos --version 2>&1 | head -1)"
    echo "client: ${client#caos: }"
} > /usr/local/share/caos/setup-stamp

exit 0
