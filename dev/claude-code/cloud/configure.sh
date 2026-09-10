#!/bin/bash
# The user-level Claude Code configuration for a cloud session: the deny list,
# the hooks, and the tool server declaration.
#
# ITS OWN SCRIPT BECAUSE TWO CALLERS NEED IT, and they need it at different
# times. `cloud/setup.sh` runs it once, before the environment is snapshotted,
# because Claude Code reads its MCP servers at startup and a session cannot
# declare one for itself. `cloud/session-start.sh` runs it again every session,
# right after it refreshes the client -- and that is not belt and braces: the
# configuration PINS the commit the client was built from, so a refreshed
# client with the old configuration would run a step from a different tree than
# the binary driving it. Whichever runs, the answer is the same function of the
# same two inputs.
#
# Takes `--base=<raw URL>`, the same one everything else here takes.
set -uo pipefail

base=""
for arg in "$@"; do
    case "$arg" in
        --base=*) base="${arg#--base=}"; base="${base%/}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done
if [ -z "$base" ]; then
    echo "FATAL: configure.sh needs --base=<raw URL>" >&2
    exit 1
fi

# The deny list, the hooks and the tool server are the SAME configuration a
# repository gets from install.sh, so they are fetched rather than restated
# here. Written out twice they agree only as long as someone remembers both,
# and the two forms do not even look alike to a reader comparing them.
#
# Three transformations, and only three:
#
#   * `${CAOS_BIN:-caos}` becomes `caos`. That indirection exists so a dev
#     checkout can point at a binary it just built; a container has one caos,
#     on PATH, and an unexpanded variable in an argv[0] is a file that does not
#     exist.
#   * a SessionStart hook is added. It is what makes this repo-independent: the
#     client finds caos through the `caos` git remote and an arbitrary checkout
#     has none, so the remote is added per session from user-level settings.
#     A repository that carries its own settings does not need it.
#   * `--llm-step:@=std/llm-step` becomes a pinned LOCATOR. See below.
if ! repo_settings="$(curl -fsSL "$base/settings.json")" \
   || ! repo_mcp="$(curl -fsSL "$base/mcp.json")"; then
    echo "FATAL: could not fetch settings.json and mcp.json from $base" >&2
    exit 1
fi

# THE STEP THAT RUNS THE TOOLS, and the reason this transformation exists.
#
# Nothing in caos knows where `llm-step` lives -- not the tui, not `cc`. It is
# an argument, and whoever launches a session is the one who answers it. The
# files fetched above answer it as `--llm-step:@=std/llm-step`, which is true of
# caos' own checkout and of nowhere else; a session here runs in SOMEBODY ELSE'S
# repository, which has no such path and no reason to grow one.
#
# So this answers it differently: a `:@@=` locator, which is another repo's tree
# pinned by a full commit sha, fetched by the client and evaluated exactly as a
# local directory would be (design/flake-inputs.md). Nothing is added to the
# session's checkout, and only the resolved oid enters the cache key, so two
# sessions pinning this commit share every image by hash.
#
# The commit is the one THIS CLIENT was built from, recorded by install.sh: the
# client and the step it drives then come from one tree, which is the only
# pairing that cannot be subtly wrong.
record=/usr/local/share/caos/build
step_repo=""
step_commit=""
if [ -r "$record" ]; then
    # READ, not sourced: this is a record to parse, and `.` would run whatever
    # is in it.
    while IFS='=' read -r key value; do
        case "$key" in
            repo) step_repo="$value" ;;
            commit) step_commit="$value" ;;
        esac
    done < "$record"
fi
if [ -z "$step_repo" ] || [ -z "$step_commit" ]; then
    echo "FATAL: the client did not record which commit it was built from," >&2
    echo "  so there is no tree to pin the step to. The installer's own" >&2
    echo "  explanation is above this line; read that, not this." >&2
    exit 1
fi
locator="--llm-step:@@=github:$step_repo?rev=$step_commit&dir=std/llm-step"
echo "the tools come from $locator" >&2

unbin='def plain: split("\"${CAOS_BIN:-caos}\"") | join("caos")
                | split("${CAOS_BIN:-caos}") | join("caos")
                | split("--llm-step:@=std/llm-step") | join($step);
       def unbin: if type == "string" then plain else . end;
       walk(unbin)'

# The hook command is run BY A SHELL, so the locator is quoted there: its `&`
# would otherwise background the hook and its `?` is a glob. An mcp `args` entry
# is argv and takes the bare form.
if ! settings="$(printf '%s' "$repo_settings" | jq --arg step "'$locator'" "$unbin"'
        | .hooks.SessionStart =
        [ { hooks: [ { type: "command", command: "caos-cloud-session-start" } ] } ]')"; then
    echo "FATAL: $base/settings.json is not the JSON this expects" >&2
    exit 1
fi
if ! servers="$(printf '%s' "$repo_mcp" | jq --arg step "$locator" "$unbin"' | .mcpServers')"; then
    echo "FATAL: $base/mcp.json is not the JSON this expects" >&2
    exit 1
fi

# A no-op substitution is the failure worth catching here: the session starts,
# every tool call dies for want of `--llm-step`, and the reason is a literal
# nobody looked at. Both files must carry it.
for configured in "$settings" "$servers"; do
    case "$configured" in
        *"$locator"*) ;;
        *)
            echo "FATAL: $base's settings/mcp do not name --llm-step:@=std/llm-step," >&2
            echo "  so there is nothing to point at the step. Is that base older" >&2
            echo "  than the client it just installed?" >&2
            exit 1
            ;;
    esac
done

changed=0
for home in /root /home/claude /home/user; do
    [ -d "$home" ] || continue
    mkdir -p "$home/.claude"

    # UNCHANGED MEANS UNTOUCHED. Claude Code re-reads a settings file live, so
    # rewriting it during a session would swap that session's hooks under it --
    # harmless when the bytes are identical, which they are on every run but the
    # one that follows a client update.
    if [ "$(cat "$home/.claude/settings.json" 2>/dev/null)" != "$settings" ]; then
        printf '%s\n' "$settings" > "$home/.claude/settings.json"
        changed=1
    fi
    chmod 0644 "$home/.claude/settings.json"

    # settings.json cannot declare an MCP server -- that lives in the user
    # config beside it. Merged rather than overwritten: the file also holds
    # account state a cloud session put there.
    cfg="$home/.claude.json"
    [ -s "$cfg" ] || echo '{}' > "$cfg"
    tmp="$cfg.caos.$$"
    if jq --argjson servers "$servers" \
         '.mcpServers = ((.mcpServers // {}) + $servers)' "$cfg" > "$tmp" 2>/dev/null; then
        if ! cmp -s "$tmp" "$cfg"; then
            cat "$tmp" > "$cfg"
            changed=1
        fi
    fi
    rm -f "$tmp"
done

# Said out loud when it happens, because of WHEN it happens: a session hook that
# rewrites the configuration is describing the NEXT session. Claude Code reads
# its MCP servers at startup, so the tool server this session is already talking
# to is the one it started with.
if [ "$changed" = 1 ]; then
    echo "wrote the user-level configuration (it takes effect next session)" >&2
fi

exit 0
