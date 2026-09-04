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
# Resolving a ref needs `jq` and two anonymous GitHub API calls. GitHub is
# already on a Claude Code cloud VM's Trusted allowlist, so this needs no
# network-policy change there.
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
gh_api() { # <path> -- fails loudly, because rate limiting is the likely one
    if ! curl -fsSL "https://api.github.com/repos/$REPO/$1"; then
        echo "GitHub API: $REPO/$1 (rate limited? private? no such ref?)" >&2
        return 1
    fi
}

if ! command -v jq >/dev/null 2>&1; then
    echo "resolving $REF needs jq" >&2
    exit 1
fi

# The newest commit at or before REF that actually HAS a build -- not REF's own
# commit. A push and an install seconds apart would find the workflow still
# running, and the previous build is a far better answer than a failure.
#
# One list of commits, one list of releases, then an intersection. `sha=` takes
# "a SHA or branch to start listing from", so a branch, a tag and a commit all
# work here and all mean the same useful thing.
have="$(gh_api "releases?per_page=100" | jq -r '.[].tag_name')"
commits="$(gh_api "commits?sha=$REF&per_page=30" | jq -r '.[].sha')"
VERSION=""
while IFS= read -r sha; do
    candidate="build-$(printf '%s' "$sha" | cut -c1-12)"
    if printf '%s\n' "$have" | grep -qxF "$candidate"; then
        VERSION="$candidate"
        break
    fi
done <<< "$commits"
if [ -z "$VERSION" ]; then
    echo "no build published for any of the last 30 commits at $REPO $REF" >&2
    echo '  (the workflow publishes build-<commit>; has it run?)' >&2
    exit 1
fi
echo "$REPO $REF -> $VERSION" >&2

# Always a named release by this point -- there is no `/releases/latest/`
# route here on purpose. GitHub's "latest" is the newest release of ANY kind,
# and with a release per push that is whichever branch pushed last, which is
# nobody's intent.
url="https://github.com/$REPO/releases/download/$VERSION/$asset"

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
cat > "$PREFIX/bin/caos" <<WRAPPER
#!/bin/sh
export CAOS_REV="\${CAOS_REV:-$stamped}"
exec -a "\$(basename "\$0")" "$PREFIX/lib/caos/caos" "\$@"
WRAPPER
chmod 0755 "$PREFIX/bin/caos"
ln -sf "$PREFIX/bin/caos" "$PREFIX/bin/caos-cli"
echo "installed $PREFIX/bin/caos ($stamped)" >&2

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
