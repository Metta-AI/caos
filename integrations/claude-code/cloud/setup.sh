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

# ---------------------------------------------------------------------------
# DEV MODE: run the caos on the SERVER, not the one the repo pins
# ---------------------------------------------------------------------------
# `caosd up` publishes its checkout to `refs/caos/dev` on the stack it starts:
# one commit carrying the working tree and the x86_64 client built from it. With
# `CAOS_DEV=1` a session runs that instead of the repo's pin, so uncommitted
# work reaches a container with no GitHub push and no wait for CI.
#
# HERE, NOT IN THE SESSION HOOK, and the reason is PROCESS LIFETIME. Claude Code
# spawns `caos mcp serve` as it starts and keeps that ONE process for the whole
# session. The overlay below replaces `/usr/local/lib/caos/caos` with `mv`,
# which swaps the directory entry for a NEW INODE -- a process already exec'd
# from the old one goes on running it. Run from the SessionStart hook, that swap
# landed ~23s AFTER Claude Code had already spawned the server, so every MCP
# tool call was served by the PINNED client for the session's entire life while
# the workers it dispatched were the dev ones. Measured: `tool_help` reached
# llm-step with no client handoff at all -- the pinned build's dispatch has no
# such branch -- and the error that produced blamed the repository's own
# expression, which was the one thing that was right.
#
# The platform supplies the ordering this needs, so it is structural rather than
# lucky: env_manager_log shows "Cloned from seed bundle" before "Running setup
# script", and "Setup script completed" strictly before "Starting Claude Code".
# It must stay BELOW the install above, which is what puts `git-remote-caos` on
# PATH and so what lets git speak `caos://` here at all.
#
# PINNED FOR THE SESSION'S LIFE: this runs for a new session and is cached on
# resume, so a resumed session keeps the client and tools it was created with
# instead of re-reading the ref. Deliberate -- swapping the caos build underneath
# a conversation already seeded and memoized against the previous one is a
# half-update, and the failure that produces reads as a code bug.
#
# EVERY PATH WRITES THE STAMP, the ones that decide not to act included. The
# session hook prints it verbatim and computes nothing, so what a session
# reports cannot drift from what happened here.
install -d /usr/local/share/caos
dev_stamp="off -- this session runs the caos its repo pins."
if [ "${CAOS_DEV:-}" = 1 ]; then
    dev_sha=""
    if [ -z "${CAOS_SERVER_URL:-}" ]; then
        dev_stamp="ASKED FOR BUT NOT ACTIVE -- CAOS_DEV=1 but no CAOS_SERVER_URL."
    elif [ -z "$repo_dir" ]; then
        dev_stamp="ASKED FOR BUT NOT ACTIVE -- no checkout here to repoint."
    else
        dev_sha="$(git -C "$repo_dir" ls-remote "$CAOS_SERVER_URL" refs/caos/dev \
                   2>/dev/null | awk '{print $1}')"
        if [ -z "$dev_sha" ]; then
            dev_stamp="ASKED FOR BUT NOT ACTIVE -- the server publishes no refs/caos/dev."
            dev_stamp="$dev_stamp Bring the stack up with 'caosd up --iroh'."
        elif ! git -C "$repo_dir" fetch --quiet --depth=1 --no-tags \
                  --no-write-fetch-head -- "$CAOS_SERVER_URL" "$dev_sha" 2>/dev/null; then
            dev_stamp="ASKED FOR BUT NOT ACTIVE -- could not fetch ${dev_sha:0:12}."
            dev_sha=""
        fi
    fi
fi
if [ -n "${dev_sha:-}" ]; then
    # THE BINARIES. Written beside and RENAMED rather than truncated in place:
    # these are executables, and overwriting a mapped file is ETXTBSY, not an
    # update. Into `lib/caos`, which is where the client looks -- `bin/caos` is
    # a wrapper that execs it, and `ensure_helper_on_path` puts `lib/caos` on
    # PATH before shelling out to git, so a helper written only into `bin` is
    # invisible to caos' own git.
    overlaid=""
    for name in caos git-remote-caos; do
        dest="/usr/local/lib/caos/$name"
        if git -C "$repo_dir" cat-file blob "$dev_sha:dev-bin/$name" > "$dest.dev" 2>/dev/null
        then
            chmod 0755 "$dest.dev" && mv -f "$dest.dev" "$dest" && overlaid="$overlaid $name"
        else
            rm -f "$dest.dev"
        fi
    done
    # AND SAY SO IN THE VERSION, because otherwise nothing does: `bin/caos` is a
    # wrapper exporting a `CAOS_REV` baked in at install time, and the overlay
    # replaces only the binary it execs -- so `caos --version` and `caos_status`
    # would go on naming the PINNED build while a different one runs.
    if [ -n "$overlaid" ] && [ -w /usr/local/bin/caos ]; then
        sed -i "s|CAOS_REV:-[^}]*}|CAOS_REV:-dev-${dev_sha:0:12}}|" /usr/local/bin/caos || true
    fi

    # THE TOOLS, from the same commit. `std/` is compiled by caos itself from the
    # std tree, so it is reached by a locator rather than installed -- and
    # `git+caos://…` is an ordinary git fetch through `git-remote-caos`, which is
    # why this needs no new transport (git-locator takes any `git+<scheme>`).
    #
    # BOTH FILES, or neither works: `std/flake-input-loader` refuses a tree whose
    # expression and `flake.lock` name different revisions, and it is
    # `flake.lock`'s URL that decides which locators it even checks.
    #
    # NOTHING HIDES THESE EDITS FROM GIT. `--skip-worktree` was tried, to stop an
    # agent committing a `caos://` URL that is also a credential, and it made dev
    # mode a no-op: caos ingests the checkout through `hash_dir`, which copies the
    # REAL index for its stat cache and so inherits that bit, and `git add -u
    # <dir>` on a skip-worktree path exits 0 and silently keeps HEAD's blob. Both
    # files reverted together, so the loader's rev-drift check saw two consistent
    # files and passed. The ticket reaches the user's own caosd either way; an
    # agent committing it upstream is a push-time concern, not a reason to lie to
    # git about what is on disk.
    tools=""
    if [ -w "$repo_dir/.caos-expr" ] && [ -r "$repo_dir/flake.lock" ]; then
        # Every `:@@=` locator repointed at this server at this commit, keeping
        # each one's own `dir=`: the expression names two (the loader's image and
        # the tree it splices) and they differ only by that.
        sed -i "s|:@@=[^ ?]*?rev=[0-9a-f]*\&dir=\([^ ]*\)|:@@=git+$CAOS_SERVER_URL?rev=$dev_sha\&dir=\1|g" \
            "$repo_dir/.caos-expr"
        # The lock's `locked` section for the caos input, found through
        # `nodes.<root>.inputs.<name>` as caos-pin.sh and the loader both do --
        # the node key is not the input name once an input has been renamed.
        # `narHash` is DROPPED rather than recomputed: an absent hash is honest,
        # where a stale one would be a lie nix would later reject.
        if tmp_lock="$(jq --arg url "$CAOS_SERVER_URL" --arg rev "$dev_sha" '
                (.root // "root") as $r
                | (.nodes[$r].inputs.caos // empty) as $k
                | if ($k|type) == "string"
                  then .nodes[$k].locked = {type:"git", url:$url, rev:$rev}
                  else . end' "$repo_dir/flake.lock" 2>/dev/null)"; then
            printf '%s\n' "$tmp_lock" > "$repo_dir/flake.lock"
            tools=1
        fi
    fi
    if [ -n "$overlaid" ] && [ -n "$tools" ]; then
        dev_stamp="ON -- client and tools from refs/caos/dev at ${dev_sha:0:12}."
    else
        dev_stamp="ASKED FOR BUT NOT ACTIVE -- refs/caos/dev ${dev_sha:0:12};"
        dev_stamp="$dev_stamp client overlay:${overlaid:- none}; tools rewrite: ${tools:+done}${tools:-NOT DONE}."
    fi
fi
printf '%s\n' "$dev_stamp" > /usr/local/share/caos/dev-stamp
echo "dev mode: $dev_stamp" >&2

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
