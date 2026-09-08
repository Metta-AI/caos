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
#   B=https://raw.githubusercontent.com/Metta-AI/caos/main/dev/claude-code
#   curl -fsSL "$B/cloud/setup.sh" | bash -s -- --base="$B"
#
# `--base` is the whole configuration: everything else -- which repo, which
# branch or commit, where the sibling scripts live -- is read back out of it. A
# script piped into bash cannot see its own URL (no $0, no path, no referrer),
# so it has to be told once, and once is all it is told.
#
# Swap `main` for a branch or a commit sha to test a change: the setup script,
# the installer, the session hook and the client then ALL come from that one
# ref, and there is no second place to keep in step.
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
base="$RAW/Metta-AI/caos/main/dev/claude-code"
for arg in "$@"; do
    case "$arg" in
        --base=*) base="${arg#--base=}"; base="${base%/}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# The repo and the ref come back out of the base, which is why there is only
# one thing to state. Shape-checked only: install.sh takes the same --base and
# does the real parse, so repeating that here would be a second rule to keep in
# step. The check is worth its four lines anyway -- a typo caught now names the
# typo, where the same typo caught later is a 404 on a URL nobody typed.
case "$base" in
    "$RAW"/*/*/*/dev/claude-code) ;;
    *)
        echo "FATAL: --base must look like" >&2
        echo "  $RAW/<owner>/<repo>/<ref>/dev/claude-code" >&2
        echo "  got: $base" >&2
        exit 1
        ;;
esac

# WHICH CLIENT: whatever --base names, passed straight through. There is no
# branch or version to choose here, because choosing one could only mean
# installing a client that does not match the scripts installing it.
#
# CAOS_IROH_TICKET is the one environment variable left, and could not be
# anything else: it is read at SESSION start, long after this has run and been
# snapshotted, so no argument here could carry it.
args="--no-repo-files --base=$base"
installer="$base/install.sh"

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
    echo "  The installer's own error is above this line; read that, not this." >&2
    exit 1
fi
caos --version >&2 2>/dev/null || true

# The iroh tunnel arrives with the client, from the same release, so there is
# nothing to install here. It has to be ours: n0's build compiles in a copy of
# Mozilla's roots and cannot reach a relay through a TLS-intercepting proxy,
# which is what a cloud container's egress is.

# The per-session work: the tunnel and the git remote. What goes in the
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
# base URL, and every session re-reads what that ref says today -- including
# the CLIENT, which the session script installs.
cat > /usr/local/bin/caos-cloud-session-start <<EOF
#!/bin/bash
base="$base"
EOF
cat >> /usr/local/bin/caos-cloud-session-start <<'BOOTSTRAP'
# Never fatal: a session that cannot reach GitHub should still start, with the
# reason on stderr, rather than be blocked by its own setup.
if ! script="$(curl -fsSL "$base/cloud/session-start.sh")"; then
    echo "caos: could not fetch $base/cloud/session-start.sh; skipping" >&2
    exit 0
fi
exec bash -c "$script" caos-cloud-session-start --base="$base"
BOOTSTRAP
chmod 0755 /usr/local/bin/caos-cloud-session-start
bash -n /usr/local/bin/caos-cloud-session-start || {
    echo "FATAL: the session-start bootstrap does not parse" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# Hooks and the tool server, user-level
# ---------------------------------------------------------------------------

# The deny list, the hooks and the tool server are the SAME configuration a
# repository gets from install.sh, so they are fetched rather than restated
# here. Written out twice they agree only as long as someone remembers both,
# and the two forms do not even look alike to a reader comparing them.
#
# Two transformations, and only two:
#
#   * `${CAOS_BIN:-caos}` becomes `caos`. That indirection exists so a dev
#     checkout can point at a binary it just built; a container has one caos,
#     on PATH, and an unexpanded variable in an argv[0] is a file that does not
#     exist.
#   * a SessionStart hook is added. It is what makes this repo-independent: the
#     client finds caos through the `caos` git remote and an arbitrary checkout
#     has none, so the remote is added per session from user-level settings.
#     A repository that carries its own settings does not need it.
if ! repo_settings="$(curl -fsSL "$base/settings.json")" \
   || ! repo_mcp="$(curl -fsSL "$base/mcp.json")"; then
    echo "FATAL: could not fetch settings.json and mcp.json from $base" >&2
    exit 1
fi

unbin='def plain: split("\"${CAOS_BIN:-caos}\"") | join("caos")
                | split("${CAOS_BIN:-caos}") | join("caos");
       def unbin: if type == "object" and (.command? | type) == "string"
                  then .command |= plain else . end;
       walk(unbin)'

if ! settings="$(printf '%s' "$repo_settings" | jq "$unbin"' | .hooks.SessionStart =
        [ { hooks: [ { type: "command", command: "caos-cloud-session-start" } ] } ]')"; then
    echo "FATAL: $base/settings.json is not the JSON this expects" >&2
    exit 1
fi
if ! servers="$(printf '%s' "$repo_mcp" | jq "$unbin"' | .mcpServers')"; then
    echo "FATAL: $base/mcp.json is not the JSON this expects" >&2
    exit 1
fi

for home in /root /home/claude /home/user; do
    [ -d "$home" ] || continue
    mkdir -p "$home/.claude"

    printf '%s\n' "$settings" > "$home/.claude/settings.json"
    chmod 0644 "$home/.claude/settings.json"

    # settings.json cannot declare an MCP server -- that lives in the user
    # config beside it. Merged rather than overwritten: the file also holds
    # account state a cloud session put there.
    cfg="$home/.claude.json"
    [ -s "$cfg" ] || echo '{}' > "$cfg"
    tmp="$cfg.caos.$$"
    if jq --argjson servers "$servers" \
         '.mcpServers = ((.mcpServers // {}) + $servers)' "$cfg" > "$tmp" 2>/dev/null; then
        cat "$tmp" > "$cfg"
    fi
    rm -f "$tmp"
done

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
    echo "client: $(caos --version 2>&1 | head -1)"
} > /usr/local/share/caos/setup-stamp

exit 0
