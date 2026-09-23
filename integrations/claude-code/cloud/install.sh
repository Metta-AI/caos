#!/bin/bash
# Install the caos Claude Code client into a repository.
#
#   C=<the 40-hex commit your client repo pins>
#   B=https://raw.githubusercontent.com/Metta-AI/caos/$C
#   curl -fsSL "$B/integrations/claude-code/cloud/install.sh" | bash -s -- --base="$B"
#
# `--base` says which caos, and is the only thing that does. It ends in a FULL
# COMMIT SHA -- not a branch, not a tag -- and installs that commit's published
# build, or fails.
#
# WHY A COMMIT AND NOTHING ELSE. A client repo pins caos in `flake.lock`, which
# records revisions, and the SAME revision rides in the root expression's
# `:@@=` locators, where `std/flake-input-loader` refuses the tree unless the
# two agree. So the step a session runs is always resolved through one specific
# commit. A branch here could only install a client from a different tree than
# its own tools -- the one pairing this file exists to keep. There is no
# fallback to an earlier build for the same reason: a written-down commit is
# honoured or refused, never quietly replaced.
#
# `setup.sh`'s `--base` is a DIFFERENT thing and may name a branch: it says
# where the bootstrap scripts come from, not which caos gets installed. It
# reads the commit out of the checkout and passes that here.
#
# There is deliberately no --branch, --commit or --version. The URL already
# names the commit, and a flag that could name a DIFFERENT one would only ever
# be used to install a client that does not match the script installing it.
#
# Saying it twice is not redundant: a script piped into bash cannot see its own
# URL -- no $0, no path, no referrer -- so it has to be told the thing it was
# just fetched from.
#
# Resolving the build needs `git` and one `ls-remote`, and downloading needs
# `curl`. Nothing here touches api.github.com: see the note above the resolution.
#
# It installs three things into the CURRENT REPOSITORY plus one binary:
#
#   /usr/local/bin/caos    the client: `caos mcp hook` and `caos mcp serve`
#   .claude/settings.json  the hooks and the deny list
#   .mcp.json              the tool server
#
# `caos` on PATH is what lets those two files be plain: they name `caos`, not a
# path into somebody's checkout.
set -euo pipefail

# Arguments only. Which build to install is not read from the environment: it
# is the kind of setting that gets exported once and then silently outranks the
# argument someone is looking straight at.
RAW="https://raw.githubusercontent.com"
BASE="$RAW/Metta-AI/caos/main"
PREFIX="${CAOS_PREFIX:-/usr/local}"
force=""
repo_files=yes
user_config=""
enable_bash=""
caos_std_path=""
dev_assets=""
seed_commit=""

for arg in "$@"; do
    case "$arg" in
        --force) force=yes ;;
        # For a cloud environment, where the configuration is user-level and
        # serves every repository: install the client and leave checkouts alone.
        --no-repo-files) repo_files="" ;;
        # A DIAGNOSTIC toggle: keep Bash/Read/Grep/Glob OUT of the deny list this
        # run writes, so a session can inspect the container directly. The caos
        # integration normally denies them so the model works through the caos
        # tools; --enable-bash trades that for visibility. Flip it by rewriting
        # the env's setup line (`... --enable-bash`), no repo change or CI needed.
        --enable-bash) enable_bash=yes ;;
        # A cloud environment's counterpart to the repo files: write the SAME
        # deny list, hooks and server declaration at the USER level, pinned to
        # the commit this run installs. This is the old configure.sh, folded in
        # because it is never wanted without an install and needs the very
        # commit the install just resolved.
        --user-config) user_config=yes ;;
        # A caos-client repo mounts caos' `std/` into its own evaluated tree
        # (std/flake-input-loader), so the step has a PATH in the checkout and
        # the configuration can name it as one: `--llm-step:@=<path>/llm-step`.
        # Without this the step is pinned by locator to the commit this client
        # was built from, which is what an arbitrary checkout needs.
        #
        # The path is the repo's `--output-path`, which `caos-pin.sh` reads out
        # of its `.caos-expr` -- not a convention this script may assume.
        --caos-std-path=*) caos_std_path="${arg#--caos-std-path=}"
                           caos_std_path="${caos_std_path%/}" ;;
        # EVERYTHING FROM A LOCAL DIRECTORY, so a dev stack can serve its own
        # install package: the client, the git helper and the two JSON assets.
        # Without it those come from a GitHub RELEASE, which means a push and a
        # CI round trip before an edit here can be seen at all.
        #
        # `curl` speaks `file://`, so pointing `url` at this directory leaves
        # every fetch below unchanged -- including the `${url%/*}/<name>` form
        # the helper and the assets use.
        --dev-assets=*) dev_assets="${arg#--dev-assets=}"; dev_assets="${dev_assets%/}" ;;
        # The commit the CONVERSATION seeds from, passed through to `mcp serve`
        # as `--base`. Without it a session's content comes from HEAD, so a dev
        # checkout whose `.caos-expr` was rewritten still evaluates the
        # COMMITTED pin -- dev client, dev step, pinned tools.
        --seed-commit=*) seed_commit="${arg#--seed-commit=}" ;;
        --base=*) BASE="${arg#--base=}"; BASE="${BASE%/}" ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# --base is the repo and the commit -- `<raw>/<owner>/<repo>/<sha>` -- and
# NOTHING more: the integration path is added by the URLs that fetch from it,
# not baked into --base, so the value reads as the base it is. Peeled by
# stripping owner and repo from the front; whatever remains is checked below to
# be a full sha.
# EVERYTHING BELOW UNTIL `url=` RESOLVES A GITHUB RELEASE, and --dev-assets
# replaces the lot: there is no release to look up, no build tag to match and
# no commit to take from one. `url` is pointed at the local directory instead,
# and because curl speaks `file://` every fetch after this is unchanged.
#
# VERSION still names the build, since it is stamped into the wrapper as
# CAOS_REV and is the only thing a session can read back to tell which client
# it is running. A dev install says so plainly rather than borrowing a build
# number it does not have.
if [ -n "$dev_assets" ]; then
    if [ ! -r "$dev_assets/caos" ]; then
        echo "FATAL: --dev-assets=$dev_assets has no caos binary in it" >&2
        exit 1
    fi
    REPO="dev"
    REF="$(cat "$dev_assets/REV" 2>/dev/null || echo unknown)"
    COMMIT=""
    VERSION="dev-${REF:0:12}"
    url="file://$dev_assets/caos"
    echo "installing from $dev_assets as $VERSION" >&2
else
    rest="${BASE#"$RAW"/}"
    owner="${rest%%/*}"; rest="${rest#*/}"
    name="${rest%%/*}";  REF="${rest#*/}"
    if [ "${BASE#"$RAW"/}" = "$BASE" ] || [ -z "$owner" ] || [ -z "$name" ] \
       || [ -z "$REF" ] || [ "$REF" = "$name" ]; then
        echo "--base must look like" >&2
        echo "  $RAW/<owner>/<repo>/<ref>" >&2
        echo "  got: $BASE" >&2
        exit 2
    fi
    REPO="$owner/$name"

    case "$(uname -s)/$(uname -m)" in
        Linux/x86_64) asset=caos-x86_64-linux ;;
        Linux/aarch64|Linux/arm64) asset=caos-aarch64-linux ;;
        *)
            echo "no published binary for $(uname -s)/$(uname -m);" >&2
            echo "build from source with \`nix build .#caos-cli\`." >&2
            exit 1
            ;;
    esac
    # VALIDATED BEFORE ANYTHING IS FETCHED. This is a pure check on an argument,
    # so paying a network round trip to be told the argument was wrong is both
    # slower and untestable -- `--base=…/main` used to `ls-remote` first and
    # only then refuse the ref.
    # A COMMIT, AND ONLY A COMMIT. `--base` names the caos this client IS, and with
    # a repo-pinned step that is never a name: `caos-pin.sh` reads it out of
    # `flake.lock`, which records revisions, and the same revision is what the
    # expression's `:@@=` locators carry -- the two must agree or
    # `std/flake-input-loader` refuses the tree. So a branch cannot reach here
    # through the supported path, and accepting one could only produce the pairing
    # this file exists to prevent: a client from a moving head driving tools
    # resolved through some other commit.
    #
    # What used to be here was a branch/tag lookup plus a walk back to the newest
    # build at or before the ref, for the window in which a branch head has no
    # build yet. Both are gone with the branch: a written-down commit is honoured
    # or refused, never quietly replaced. The cost is stated in the READMEs --
    # push, wait for `build-<commit>`, then re-pin.
    case "$REF" in
        *[!0-9a-f]* | "")
            echo "--base must end in a full commit sha, not $REF" >&2
            echo "  A client repo pins caos by revision (flake.lock), and the step" >&2
            echo "  resolves through that same revision, so a branch or tag here" >&2
            echo "  would install a client from a different tree than its tools." >&2
            exit 2
            ;;
    esac
    if [ "${#REF}" != 40 ]; then
        echo "--base must end in a FULL 40-character commit sha; got ${#REF} characters" >&2
        echo "  ($REF). That is what flake.lock records and what the expression pins." >&2
        exit 2
    fi


    # A build is named by its COMMIT -- `build-<12 hex>` -- and by nothing else, so
    # resolving one is a lookup rather than a parse.
    #
    # Read with `git ls-remote`, NOT api.github.com. That API is anonymous here, so
    # it is rate limited to 60 requests an hour PER IP -- and a cloud VM shares its
    # egress address with every other cloud VM, so the budget is spent by strangers
    # and the 403 is nothing this side can fix. ls-remote has no such limit, needs
    # no token, and speaks to github.com like the download does.
    if ! command -v git >/dev/null 2>&1; then
        echo "resolving the build for $REF needs git" >&2
        exit 1
    fi
    remote="https://github.com/$REPO"
    if ! refs="$(git ls-remote "$remote" 2>&1)"; then
        echo "could not list the refs of $remote:" >&2
        printf '%s\n' "$refs" | head -3 >&2
        exit 1
    fi


    builds=""
    while IFS=$'\t' read -r s r; do
        case "$r" in refs/tags/build-*) builds="$builds${r#refs/tags/}"$'\n' ;; esac
    done <<< "$refs"

    # A build is named `build-<first twelve of the commit>`, so this is a lookup,
    # not a search: truncate the ref and ask whether that tag exists.
    VERSION=""
    ref12="${REF:0:12}"
    while IFS= read -r b; do
        case "${b#build-}" in
            "$ref12") VERSION="$b"; break ;;
        esac
    done <<< "$builds"
    if [ -z "$VERSION" ]; then
        echo "$REPO has no build for $REF, and a pinned commit does not fall back" >&2
        echo "  to an earlier one: the step resolves through THIS rev, so an older" >&2
        echo "  client would drive tools built from a different tree." >&2
        echo "  The workflow publishes build-<commit> and may still be running --" >&2
        echo "  wait for it, then re-pin. \`gh run list\` says when." >&2
        exit 1
    fi
    echo "$REPO $REF -> $VERSION" >&2

    # The FULL commit this build came from, which a build tag's twelve hex digits
    # are not. Whoever configures a session has to name the step that runs its
    # tools, and outside the caos checkout that name is a pinned locator
    # (`--llm-step:@@=github:<repo>?rev=<40 hex>&dir=std/llm-step`) whose rev is
    # mandatory and unabbreviated. Taken from the ref listing already in hand.
    #
    # AND CHECKED AGAINST THE TAG'S OWN NAME, because the tag has been wrong. The
    # Releases API creates a missing tag at the default branch's head unless the
    # workflow passes `target_commitish`, and it did not until 2026-09-09 -- so
    # every build tag published before that names one commit and points at another
    # (whatever main's head was). Nothing read the target then; this does, and a
    # silently wrong pin would hand a session a step built from an unrelated tree.
    COMMIT=""
    while IFS=$'\t' read -r s r; do
        case "$r" in
            # Peeled first: an annotated tag's own object is not what was built.
            "refs/tags/$VERSION^{}") COMMIT="$s"; break ;;
            "refs/tags/$VERSION") COMMIT="$s" ;;
        esac
    done <<< "$refs"
    if [ -z "$COMMIT" ]; then
        echo "$REPO has no tag $VERSION to take a commit from" >&2
        exit 1
    fi
    # A mismatch drops the commit rather than failing: installing the CLIENT does
    # not need it, and refusing over it would take out the whole install for a
    # build that is otherwise perfectly good. What cannot proceed is naming the
    # step by locator, and that is where the refusal belongs -- in the caller that
    # reads this record and finds no commit in it.
    case "$VERSION" in
        build-*)
            if [ "${VERSION#build-}" != "${COMMIT:0:12}" ]; then
                echo "$REPO tag $VERSION points at $COMMIT, a different commit," >&2
                echo "  so this build cannot say which tree it came from. It predates" >&2
                echo "  the workflow fix that puts a build tag on the commit it names;" >&2
                echo "  a session that has to name the step needs a newer build." >&2
                COMMIT=""
            fi
            ;;
    esac

    # Always a named release by this point -- there is no `/releases/latest/`
    # route here on purpose. GitHub's "latest" is the newest release of ANY kind,
    # and with a release per push that is whichever branch pushed last, which is
    # nobody's intent.
    url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

# A function, purely so the "already current" test below can skip it without
# indenting a heredoc -- a `WRAPPER` terminator moved off column 0 swallows the
# rest of the file, and an indented `#!` is not a shebang at all.
install_client() {
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "fetching $url" >&2
curl -fsSL "$url" -o "$tmp/caos"
chmod +x "$tmp/caos"

# The published binary is the STATIC one, without the version wrapper nix adds,
# so on its own it reports an empty rev. A small wrapper puts the build back:
# telling a stale client from a current one is the only reason it prints at all.
# It is the build that was resolved, so there is nothing for the release
# workflow to stamp -- a placeholder here could only ever disagree with it.
stamped="$VERSION"
install -d "$PREFIX/bin" "$PREFIX/lib/caos"
install -m 0755 "$tmp/caos" "$PREFIX/lib/caos/caos"
# `#!/bin/bash`, NOT `#!/bin/sh`: `exec -a` is a bash builtin and /bin/sh is
# dash on Debian and Ubuntu, where it is `exec: -a: not found` on every call.
cat > "$PREFIX/bin/caos" <<WRAPPER
#!/bin/bash
export CAOS_REV="\${CAOS_REV:-$stamped}"
exec -a "\$(basename "\$0")" "$PREFIX/lib/caos/caos" "\$@"
WRAPPER
chmod 0755 "$PREFIX/bin/caos"
ln -sf "$PREFIX/bin/caos" "$PREFIX/bin/caos-cli"

# RUN IT. A wrapper is shell written by shell, and whether it executes is not
# implied by having written it: a `caos` on PATH that fails on every call still
# looks like a successful install.
#
# Assert the OUTPUT, not the exit code: `caos` with no arguments prints usage
# and exits NON-ZERO, which is its contract, so testing the status would fail
# for a binary that is working perfectly.
banner="$("$PREFIX/bin/caos" 2>&1 || true)"
case "$banner" in
    *"usage:"*) ;;
    *)
        echo "the installed client does not run:" >&2
        printf '%s\n' "$banner" | head -3 >&2
        exit 1
        ;;
esac
echo "installed $PREFIX/bin/caos ($stamped)" >&2

# The git remote helper, from the SAME release and into the SAME directory.
#
# Both halves matter. A `caos://` server is reached by the client directly, but
# `git` reaches it by EXECING `git-remote-caos` off PATH, and caos shells out to
# git for every push and fetch — so a client without this one beside it resolves
# a server it then cannot push to. And the client finds it by looking in its own
# directory (`ensure_helper_on_path`), which is why "the same directory" is the
# requirement rather than "somewhere on PATH".
#
# Not fatal when absent: a build from before this existed has no such asset, and
# a client that can still reach an http:// server beats no client at all.
if curl -fsSL "${url%/*}/git-remote-caos-x86_64-linux" -o "$tmp/git-remote-caos" 2>/dev/null; then
    # BESIDE THE REAL BINARY, not beside the wrapper, and that distinction is
    # the whole of it: the client puts its OWN directory on PATH before shelling
    # out to git (`ensure_helper_on_path`), and its own directory is
    # `lib/caos` — `bin/caos` is a shell wrapper that execs it. A helper only in
    # `bin` is therefore invisible to the client's own git unless something else
    # happens to have put `bin` on PATH, which is how this first failed: a
    # session resolved its server, pushed, and died on
    # `git: 'remote-caos' is not a git command`.
    #
    # The symlink in `bin` is for a person typing it and for a git that inherits
    # an ordinary PATH. One copy, two names, the same shape the client itself is
    # installed with.
    install -m 0755 "$tmp/git-remote-caos" "$PREFIX/lib/caos/git-remote-caos"
    ln -sf "$PREFIX/lib/caos/git-remote-caos" "$PREFIX/bin/git-remote-caos"
    echo "installed $PREFIX/lib/caos/git-remote-caos (linked into $PREFIX/bin)" >&2
else
    echo "no git-remote-caos in $VERSION; a caos:// server will not work" >&2
fi
}

# Already current? Then skip the DOWNLOAD -- not the repo files below. This
# runs at EVERY session start, because a client installed by the setup script
# is frozen into the environment's snapshot and a push never reaches it, so the
# ordinary case has to cost one `ls-remote` and no transfer. The wrapper
# records the build it installed, which makes that answerable without hashing
# anything. `--force` reinstalls regardless, for when the binary is suspect.
#
# The helper counts as part of "installed": a prefix holding a current client
# and no `git-remote-caos` would otherwise skip the download that fixes it.
#
# Asked of `lib/caos`, where the CLIENT looks, not of `bin`. An environment set
# up by the version that installed it only into `bin` has a current client and a
# helper its own git cannot find, and asking the wrong question there would skip
# the download that repairs it on the next session.
if [ -z "$force" ] && [ -x "$PREFIX/bin/caos" ] \
   && [ -x "$PREFIX/lib/caos/git-remote-caos" ] \
   && grep -qF "CAOS_REV:-$VERSION}" "$PREFIX/bin/caos" 2>/dev/null; then
    echo "$PREFIX/bin/caos is already $VERSION" >&2
else
    install_client
fi

# WHAT THIS CLIENT IS, for whoever has to name the step it drives. Written on
# every run, including the skipped-download path: it describes the resolution,
# not the transfer, and a prefix that has the binary but not this record would
# leave the reader with the twelve digits in the wrapper and no way to expand
# them. `write_user_config` below reads $REPO/$COMMIT (the same values) to build
# the locator it pins into a session's configuration.
install -d "$PREFIX/share/caos"
cat > "$PREFIX/share/caos/build" <<RECORD
repo=$REPO
commit=$COMMIT
version=$VERSION
RECORD
chmod 0644 "$PREFIX/share/caos/build"

# THE MCP ENTRY POINT, so the RUNNING tool server is never a stale binary.
#
# A cloud environment restored from a cache brings back the client the LAST
# FRESH setup installed and skips the setup that would refresh it; the
# SessionStart hook does refresh, but its `install.sh` lands AFTER Claude Code
# has already spawned `mcp serve` from the old binary, so the session runs stale
# (its tools too). This wrapper moves the refresh to the LAUNCH: the user-level
# MCP declaration points the caos server's command at it, so before the server
# starts it reinstalls `$BASE`'s build and then execs the client. `$BASE` is a
# COMMIT and is baked in -- the wrapper cannot read its own argv for it any
# more than this script can -- so "refresh" means "make sure this exact build
# is installed", which is a no-op whenever it already is. Bounded and non-fatal:
# a slow or unreachable GitHub serves the installed client rather than hanging
# startup. Its refresh output is forced to STDERR, because the wrapper's stdout
# becomes the tool server's JSON-RPC the moment it execs.
# WRITTEN TO A TEMPORARY AND MOVED INTO PLACE, never truncated where it stands.
# This script is what `caos-serve` re-runs, so the file being rewritten is the
# one bash is CURRENTLY READING -- and bash reads a script lazily, by byte
# offset, so truncating it mid-run makes it resume at an offset into different
# text. A rename swaps the inode and leaves the running shell on the old one.
#
# `--caos-std-path` rides in the refresh command for the same reason the step
# does: without it the refresh would rewrite this wrapper in the LOCATOR form,
# silently repointing the next launch at the step the client was built from
# rather than the one the repo pins.
serve_tmp="$PREFIX/bin/.caos-serve.$$"
# NO REFRESH IN DEV MODE, and this is not an optimisation. The refresh exists to
# un-freeze a snapshot's client by re-installing from GitHub; run after a dev
# install it would replace the binary that was just taken from the dev server
# with the one the repo pins -- undoing dev mode a few seconds after setup
# established it, at the moment the tool server starts. The dev client is
# current by construction: setup.sh installed it from this session's fetch.
if [ -n "$dev_assets" ]; then
    cat > "$serve_tmp" <<WRAP
#!/bin/bash
WRAP
else
cat > "$serve_tmp" <<WRAP
#!/bin/bash
timeout 20 bash -c "curl -fsSL '$BASE/integrations/claude-code/cloud/install.sh' | bash -s -- --no-repo-files --base='$BASE'${caos_std_path:+ --caos-std-path='$caos_std_path'}" >&2 || echo "caos-serve: client refresh skipped (failed or timed out); using the installed one" >&2
WRAP
fi
if [ -n "$caos_std_path" ]; then
    # A REPO-PINNED step needs nothing from the build record: the path names the
    # caos the checkout itself mounts, so the refresh above cannot move it out
    # from under the serve. The client resolves it by descending through the
    # root `.caos-expr` from the checkout (`resolve_cli_image_with_store`
    # eval-paths the ingested workspace), which is what makes a path that exists
    # only in the EVALUATION result nameable here.
    cat >> "$serve_tmp" <<WRAP
exec "$PREFIX/bin/caos" mcp serve "--llm-step:@=$caos_std_path/llm-step"${seed_commit:+ "--base=$seed_commit"}
WRAP
else
    cat >> "$serve_tmp" <<WRAP
# The step, pinned to the commit the refresh JUST installed -- not the one the
# snapshot's mcp.json named. Refreshing the binary without this would run the new
# server against an OLD llm-step (its tools are the pinned rev's), which is the
# half-update that looks like the fix not working. Built the same way
# configure.sh builds it, from the build record install.sh just wrote.
r="\$(sed -n 's/^repo=//p' "$PREFIX/share/caos/build" 2>/dev/null)"
c="\$(sed -n 's/^commit=//p' "$PREFIX/share/caos/build" 2>/dev/null)"
if [ -n "\$r" ] && [ -n "\$c" ]; then
    exec "$PREFIX/bin/caos" mcp serve "--llm-step:@@=github:\$r?rev=\$c&dir=std/llm-step"
fi
exec "$PREFIX/bin/caos" "\$@"
WRAP
fi
chmod 0755 "$serve_tmp"
mv -f "$serve_tmp" "$PREFIX/bin/caos-serve"

# The repository files. NOT overwritten without --force: a checkout that already
# has `.claude/settings.json` has someone's configuration in it, and replacing
# that silently is how a deny list nobody asked for disarms their session.
root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
place() { # <relative path> <asset name>
    local dest="$root/$1" name="$2"
    if [ -e "$dest" ] && [ -z "$force" ]; then
        echo "keeping existing $1 (pass --force to replace it)" >&2
        return
    fi
    mkdir -p "$(dirname "$dest")"
    curl -fsSL "${url%/*}/$name" -o "$dest"
    echo "wrote $1" >&2
}
if [ -n "$repo_files" ]; then
    place .claude/settings.json claude-settings.json
    place .mcp.json mcp.json
fi

if [ -n "$repo_files" ]; then
    cat >&2 <<'DONE'

Done. Point the client at a caos server, then start a session:

  git remote add caos <server-url>        # or set CAOS_SERVER_URL
    # <server-url> is an http:// URL, or the caos:// ticket
    # `caosd ticket` prints on the machine running the server
  claude

`caos mcp serve` is spawned by Claude Code from .mcp.json; the hooks in
.claude/settings.json record the conversation.

Both name the step whose tools the session offers. Nothing defaults it, so the
files you were just given say `--llm-step:@=std/llm-step`, which is caos' own
path and is wrong for this repository unless it IS caos. Either point it at
wherever this tree reaches caos (`--llm-step:@=caos-std/llm-step`), or name no
path at all and let the client fetch the tree:
DONE
    # Outside the heredoc because that one is quoted -- it has to be, it is full
    # of backticks -- and this line is the only part with anything to expand.
    if [ -n "$COMMIT" ]; then
        echo "  --llm-step:@@=github:$REPO?rev=$COMMIT&dir=std/llm-step" >&2
        echo >&2
        echo "That pins the commit this client was built from, so the two agree." >&2
    else
        echo "  (this build cannot say which commit it came from -- see above --" >&2
        echo "   so it cannot offer you that spelling; use a newer one.)" >&2
    fi
else
    echo "installed the client only; no repository files were written" >&2
fi

# ---------------------------------------------------------------------------
# The user-level configuration -- folded in from the old configure.sh
# ---------------------------------------------------------------------------
# A cloud session's config is USER-level (a container serves every repository),
# so it is written here rather than into a checkout, and pinned to the commit
# THIS run just installed -- the one pairing of client and step that cannot be
# subtly wrong. Two callers reach it, both as `--no-repo-files --user-config`:
# cloud/setup.sh once before the snapshot (Claude Code reads its MCP servers at
# startup and a session cannot declare one for itself), and cloud/session-start.sh
# each session right after the refresh (a refreshed client with the old config
# would drive a step from a different tree than the binary driving it).
write_user_config() {
    # The commit is needed only by the LOCATOR form. A repo-pinned step names a
    # path in the checkout instead, so a build that cannot say which tree it
    # came from is no obstacle -- the repo says, which is the whole point.
    if [ -z "$caos_std_path" ] && { [ -z "$COMMIT" ] || [ -z "$REPO" ]; }; then
        echo "FATAL: this build cannot say which commit it came from, so there is" >&2
        echo "  no tree to pin the step to and the config would be useless." >&2
        echo "  (A caos-client repo avoids this: pass --caos-std-path=<path> and" >&2
        echo "   the step is named by a path in the checkout instead.)" >&2
        exit 1
    fi
    local raw_settings raw_mcp locator settings servers configured home cfg tmp changed settings_prog
    if ! raw_settings="$(curl -fsSL "${url%/*}/claude-settings.json")" \
       || ! raw_mcp="$(curl -fsSL "${url%/*}/mcp.json")"; then
        echo "FATAL: could not fetch the config assets from this build's release" >&2
        exit 1
    fi
    if [ -n "$caos_std_path" ]; then
        # A caos-client repo MOUNTS caos' std into its evaluated tree, so the
        # step is an ordinary path and the client resolves it by descent -- the
        # same walk that reaches `DEEP-DEPS/<x>` inside caos itself. This is
        # also what makes `reader=$caos_std_path/llm-step` resolvable in a
        # committed `.caos-secrets` entry: readers are eval-path'd against the
        # same tree, and a reader naming a path the tree lacks grants nothing.
        locator="--llm-step:@=$caos_std_path/llm-step"
    else
        # A `:@@=` locator pins the step to another repo's tree by full sha,
        # fetched by the client and evaluated like a local directory -- so an
        # arbitrary checkout needs no std/llm-step of its own, and the step comes
        # from the SAME tree the client was built from.
        locator="--llm-step:@@=github:$REPO?rev=$COMMIT&dir=std/llm-step"
    fi
    echo "the tools come from $locator" >&2
    local unbin='def plain: split("\"${CAOS_BIN:-caos}\"") | join("caos")
                    | split("${CAOS_BIN:-caos}") | join("caos")
                    | split("--llm-step:@=std/llm-step") | join($step);
           def unbin: if type == "string" then plain else . end;
           walk(unbin)'
    # In the hook command the locator is a shell word (quote its `&`/`?`); in an
    # mcp `args` entry it is bare argv. A SessionStart hook is added: the client
    # finds caos through a `caos` git remote an arbitrary checkout lacks, so the
    # remote is added per session from user-level settings.
    settings_prog="$unbin"'
            | .hooks.SessionStart =
            [ { hooks: [ { type: "command", command: "caos-cloud-session-start" } ] } ]'
    if [ -n "$enable_bash" ]; then
        # Lift the read/inspect tools out of deny and into allow, so a session can
        # be driven to dump the container. Edit/Write/NotebookEdit/Monitor stay
        # denied -- this is for looking, not for the model rewriting the checkout.
        settings_prog="$settings_prog"'
            | .permissions.deny  = ((.permissions.deny  // []) - ["Bash","Read","Grep","Glob"])
            | .permissions.allow = ((.permissions.allow // []) + ["Bash","Read","Grep","Glob"] | unique)'
        echo "--enable-bash: Bash/Read/Grep/Glob will be allowed this session" >&2
    fi
    if ! settings="$(printf '%s' "$raw_settings" | jq --arg step "'$locator'" "$settings_prog")"; then
        echo "FATAL: the settings asset is not the JSON this expects" >&2
        exit 1
    fi
    # command becomes `caos-serve`, the refresh-then-exec wrapper installed above,
    # so a snapshot's frozen binary is still current when it serves.
    if ! servers="$(printf '%s' "$raw_mcp" \
        | jq --arg step "$locator" "$unbin"' | .mcpServers | .caos.command = "caos-serve"')"; then
        echo "FATAL: the mcp asset is not the JSON this expects" >&2
        exit 1
    fi
    # A no-op substitution is the failure worth catching: the session starts and
    # every tool call dies for want of --llm-step, and the reason is a literal
    # nobody looked at.
    for configured in "$settings" "$servers"; do
        case "$configured" in
            *"$locator"*) ;;
            *)
                echo "FATAL: the config assets do not name --llm-step, so nothing" >&2
                echo "  points at the step. Is this base older than the client?" >&2
                exit 1
                ;;
        esac
    done
    changed=0
    for home in /root /home/claude /home/user; do
        [ -d "$home" ] || continue
        mkdir -p "$home/.claude"
        # UNCHANGED MEANS UNTOUCHED: Claude Code re-reads a settings file live, so
        # rewriting identical bytes mid-session would needlessly swap its hooks.
        if [ "$(cat "$home/.claude/settings.json" 2>/dev/null)" != "$settings" ]; then
            printf '%s\n' "$settings" > "$home/.claude/settings.json"
            changed=1
        fi
        chmod 0644 "$home/.claude/settings.json"
        # settings.json cannot declare an MCP server -- that lives in the user
        # config beside it, MERGED (it also holds account state a session put there).
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
    # `if`, not `&& echo`: as the last statement in this function a false test
    # would return non-zero and, under the caller's `set -e`, exit install.sh.
    if [ "$changed" = 1 ]; then
        echo "wrote the user-level configuration (it takes effect next session)" >&2
    fi
}

if [ -n "$user_config" ]; then
    write_user_config
fi
