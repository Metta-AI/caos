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
# resolving one is a lookup rather than a parse. It was `<branch>-<sha>` once,
# and since `-` is legal in a branch name, `cc` and `cc-conversations` made tags
# no rule could separate; asking GitHub what is on a ref is the exact question
# that name was a lossy encoding of.
# Resolved with `git ls-remote`, NOT api.github.com. The API is anonymous here,
# so it is rate limited to 60 requests an hour PER IP -- and a cloud VM shares
# its egress address with every other cloud VM, so the budget is spent by
# strangers. It failed exactly that way in a Claude Code cloud session:
# raw.githubusercontent.com served the script and then api.github.com answered
# 403, which the setup reported as "a release has to EXIST" -- blaming the one
# thing that was fine.
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

if [ -n "$sha" ]; then
    VERSION="build-${sha:0:12}"
    case "$builds" in
        *"$VERSION"$'\n'*) ;;
        *)
            echo "$REPO $REF is $sha, which has no build yet" >&2
            echo "  (the workflow publishes build-<commit>; is it still running?)" >&2
            exit 1
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
# `#!/bin/bash`, NOT `#!/bin/sh`: `exec -a` is a bash builtin, and /bin/sh is
# dash on Debian and Ubuntu. Written as sh, every invocation died with
# `/usr/local/bin/caos: 3: exec: -a: not found` -- so in a cloud container the
# client was never able to run AT ALL, and the visible symptom was the MCP tool
# server reporting CONNECTION_CLOSED, which reads as a network problem.
cat > "$PREFIX/bin/caos" <<WRAPPER
#!/bin/bash
export CAOS_REV="\${CAOS_REV:-$stamped}"
exec -a "\$(basename "\$0")" "$PREFIX/lib/caos/caos" "\$@"
WRAPPER
chmod 0755 "$PREFIX/bin/caos"
ln -sf "$PREFIX/bin/caos" "$PREFIX/bin/caos-cli"

# RUN IT. A wrapper is three lines of shell written by another shell, and the
# one thing never checked was whether it executes -- `exec -a` under dash left
# a `caos` on PATH that failed on every call, and the install said "installed".
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
}

# Already current? Then skip the DOWNLOAD -- not the repo files below. This now
# runs at EVERY session start, because a client installed by the setup script
# is frozen into the environment's snapshot and a push never reaches it. So the
# ordinary case has to cost one `ls-remote` and no transfer.
#
# The wrapper records the build it installed, which makes the question
# answerable by reading three lines of shell. `--force` reinstalls regardless,
# which is what to reach for when the binary itself is suspect.
if [ -z "$force" ] && [ -x "$PREFIX/bin/caos" ] \
   && grep -qF "CAOS_REV:-$VERSION}" "$PREFIX/bin/caos" 2>/dev/null; then
    echo "$PREFIX/bin/caos is already $VERSION" >&2
else
    install_client
fi

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
.claude/settings.json record the conversation. Neither needs a path.
DONE
else
    echo "installed the client only; no repository files were written" >&2
fi
