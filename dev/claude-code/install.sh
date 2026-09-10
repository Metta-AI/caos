#!/bin/bash
# Install the caos Claude Code client into a repository.
#
#   B=https://raw.githubusercontent.com/Metta-AI/caos/main/dev/claude-code
#   curl -fsSL "$B/install.sh" | bash -s -- --base="$B"
#
# `--base` says which caos, and is the only thing that does. It names a repo and
# a ref -- a branch, a tag or a sha -- and the client installed is the newest
# build at or before that point. Swap `main` for anything else to install from
# there.
#
# There is deliberately no --branch, --commit or --version. The URL already
# names a ref, and a flag that could name a DIFFERENT one would only ever be
# used to install a client that does not match the script installing it.
#
# Saying it twice is not redundant: a script piped into bash cannot see its own
# URL -- no $0, no path, no referrer -- so it has to be told the thing it was
# just fetched from.
#
# Resolving a ref needs `git` and one `ls-remote`, and downloading needs `curl`.
# Nothing here touches api.github.com: see the note above the resolution.
#
# It installs three things into the CURRENT REPOSITORY plus one binary:
#
#   /usr/local/bin/caos    the client: `caos cc hook` and `caos cc serve`
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
BASE="$RAW/Metta-AI/caos/main/dev/claude-code"
PREFIX="${CAOS_PREFIX:-/usr/local}"
force=""
repo_files=yes

for arg in "$@"; do
    case "$arg" in
        --force) force=yes ;;
        # For a cloud environment, where the configuration is user-level and
        # serves every repository: install the client and leave checkouts alone.
        --no-repo-files) repo_files="" ;;
        --base=*) BASE="${arg#--base=}"; BASE="${BASE%/}" ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

# Peeled from BOTH ends rather than by field number: the trailing
# `/dev/claude-code` is fixed, so whatever is left in the middle is the ref --
# which is what makes a slashed branch (`feature/x`) work, where counting
# fields would silently take `feature` and resolve against the wrong tree.
rest="${BASE#"$RAW"/}"
owner="${rest%%/*}"; rest="${rest#*/}"
name="${rest%%/*}";  rest="${rest#*/}"
REF="${rest%/dev/claude-code}"
if [ "$BASE" = "$rest" ] || [ -z "$owner" ] || [ -z "$name" ] || [ -z "$REF" ]; then
    echo "--base must look like" >&2
    echo "  $RAW/<owner>/<repo>/<ref>/dev/claude-code" >&2
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

# A build is named by its COMMIT -- `build-<12 hex>` -- and by nothing else, so
# resolving one is a lookup rather than a parse. A name carrying the branch
# cannot be taken apart again: `-` is legal in a branch name, so `cc` and
# `cc-conversations` produce tags no rule can separate.
#
# Resolved with `git ls-remote`, NOT api.github.com. That API is anonymous
# here, so it is rate limited to 60 requests an hour PER IP -- and a cloud VM
# shares its egress address with every other cloud VM, so the budget is spent
# by strangers and the 403 is nothing this side can fix.
#
# ls-remote has no such limit, needs no token, speaks to github.com like the
# download does, and answers both halves of the question at once: the refs it
# lists include the branch heads AND every `build-<commit>` tag.
if ! command -v git >/dev/null 2>&1; then
    echo "resolving $REF needs git" >&2
    exit 1
fi
remote="https://github.com/$REPO"
if ! refs="$(git ls-remote "$remote" 2>&1)"; then
    echo "could not list the refs of $remote:" >&2
    printf '%s\n' "$refs" | head -3 >&2
    exit 1
fi

# Looked up before being guessed at: a name that IS a branch or a tag is one,
# and only a name that is neither gets treated as a commit. Deciding by shape
# instead would mean asking whether a string looks like a sha, and a branch may
# be named anything at all.
sha=""
while IFS=$'\t' read -r s r; do
    case "$r" in
        # A peeled annotated tag is the commit; the unpeeled ref is the tag
        # object, which nothing was ever built from.
        "refs/tags/$REF^{}") sha="$s"; break ;;
        "refs/heads/$REF"|"refs/tags/$REF") sha="$s" ;;
    esac
done <<< "$refs"

builds=""
while IFS=$'\t' read -r s r; do
    case "$r" in refs/tags/build-*) builds="$builds${r#refs/tags/}"$'\n' ;; esac
done <<< "$refs"

# THE NEWEST BUILD AT OR BEFORE A COMMIT, which is what the header at the top
# of this file has always promised and what the code never did.
#
# A branch head has no build for the ~25 minutes its workflow takes, and in that
# window every environment built from that branch failed to install a client AT
# ALL -- one push, and the next cloud session comes up with no caos in it. A
# slightly older client is a different thing from no client.
#
# Needs history, which `ls-remote` does not carry, so it shallow-fetches the ref
# and walks back. Fifty is a bound, not a guess: past that, something other than
# "CI is still running" is wrong, and saying so is more use than reaching
# further back.
WALK_DEPTH=50
newest_build_at_or_before() { # <ref-or-sha> ; prints build-<12 hex>
    local ref=$1 dir walk commit short
    dir="$(mktemp -d)"
    if ! git -C "$dir" init -q . 2>/dev/null \
        || ! git -C "$dir" fetch -q --depth "$WALK_DEPTH" "$remote" "$ref" 2>/dev/null; then
        rm -rf "$dir"
        return 1
    fi
    walk="$(git -C "$dir" log --format=%H FETCH_HEAD 2>/dev/null)"
    rm -rf "$dir"
    [ -n "$walk" ] || return 1
    while IFS= read -r commit; do
        short="${commit:0:12}"
        case "$builds" in
            *"build-$short"$'\n'*) printf 'build-%s\n' "$short"; return 0 ;;
        esac
    done <<< "$walk"
    return 1
}

if [ -n "$sha" ]; then
    VERSION="build-${sha:0:12}"
    case "$builds" in
        *"$VERSION"$'\n'*) ;;
        *)
            echo "$REPO $REF is $sha, which has no build yet" >&2
            echo "  (the workflow publishes build-<commit>; it may still be running)" >&2
            if ! VERSION="$(newest_build_at_or_before "$REF")"; then
                echo "  and no build exists in its last $WALK_DEPTH commits either" >&2
                exit 1
            fi
            echo "  falling back to $VERSION, the newest build at or before it" >&2
            ;;
    esac
else
    # Neither a branch nor a tag, so a commit -- and possibly an abbreviated
    # one. The build tags carry 12 hex digits, so a shorter sha is a prefix of
    # exactly the tag wanted, and nothing else has to expand it.
    VERSION=""
    while IFS= read -r b; do
        case "${b#build-}" in "$REF"*) VERSION="$b"; break ;; esac
    done <<< "$builds"
    if [ -z "$VERSION" ]; then
        echo "$REPO has no branch, tag or built commit called $REF" >&2
        exit 1
    fi
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

# The iroh tunnel, from the SAME release. Ours, not n0's: iroh compiles in a
# copy of Mozilla's roots, and where egress is a TLS-intercepting proxy the
# relay presents that proxy's certificate -- so n0's build reaches no relay at
# all. See dev/claude-code/dumbpipe-system-certs.patch.
#
# Not fatal when absent: a build from before this existed has no such asset,
# and a client that works without a tunnel is better than no client. The
# session hook already says plainly when CAOS_IROH_TICKET is set and the tunnel
# is missing.
if curl -fsSL "${url%/*}/dumbpipe-x86_64-linux" -o "$tmp/dumbpipe" 2>/dev/null; then
    install -m 0755 "$tmp/dumbpipe" "$PREFIX/bin/dumbpipe"
    echo "installed $PREFIX/bin/dumbpipe" >&2
else
    echo "no tunnel in $VERSION; CAOS_IROH_TICKET will not work" >&2
fi
}

# Already current? Then skip the DOWNLOAD -- not the repo files below. This
# runs at EVERY session start, because a client installed by the setup script
# is frozen into the environment's snapshot and a push never reaches it, so the
# ordinary case has to cost one `ls-remote` and no transfer. The wrapper
# records the build it installed, which makes that answerable without hashing
# anything. `--force` reinstalls regardless, for when the binary is suspect.
#
# The tunnel counts as part of "installed": a prefix holding a current client
# and no tunnel would otherwise skip the download that fixes it.
if [ -z "$force" ] && [ -x "$PREFIX/bin/caos" ] && [ -x "$PREFIX/bin/dumbpipe" ] \
   && grep -qF "CAOS_REV:-$VERSION}" "$PREFIX/bin/caos" 2>/dev/null; then
    echo "$PREFIX/bin/caos is already $VERSION" >&2
else
    install_client
fi

# WHAT THIS CLIENT IS, for whoever has to name the step it drives. Written on
# every run, including the skipped-download path: it describes the resolution,
# not the transfer, and a prefix that has the binary but not this record would
# leave the reader with the twelve digits in the wrapper and no way to expand
# them. `dev/claude-code/cloud/configure.sh` reads it to build the locator it
# writes into a session's configuration.
install -d "$PREFIX/share/caos"
cat > "$PREFIX/share/caos/build" <<RECORD
repo=$REPO
commit=$COMMIT
version=$VERSION
RECORD
chmod 0644 "$PREFIX/share/caos/build"

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
  claude

`caos cc serve` is spawned by Claude Code from .mcp.json; the hooks in
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
