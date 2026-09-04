#!/bin/bash
# Install the caos Claude Code client into a repository.
#
#   curl -fsSL <release-url>/install.sh | bash
#   curl -fsSL <release-url>/install.sh | bash -s -- --force
#
# WHICH BUILD, in increasing order of precedence:
#
#   (nothing)                 the newest release of any kind
#   --branch=X  / CAOS_BRANCH the newest release built from branch X
#   --version=T / CAOS_VERSION exactly release T
#
# Published as a release asset beside the binary it fetches, so one URL is
# enough to provision a machine that has never seen caos -- including a Claude
# Code cloud VM, whose network allowlist already trusts GitHub.
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

REPO="${CAOS_REPO:-Metta-AI/caos}"
VERSION="${CAOS_VERSION:-latest}"
BRANCH="${CAOS_BRANCH:-}"
PREFIX="${CAOS_PREFIX:-/usr/local}"
force=""
repo_files=yes

for arg in "$@"; do
    case "$arg" in
        --force) force=yes ;;
        # For a cloud environment, where the configuration is user-level and
        # serves every repository: install the client and leave checkouts alone.
        --no-repo-files) repo_files="" ;;
        --version=*) VERSION="${arg#--version=}" ;;
        --branch=*) BRANCH="${arg#--branch=}" ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) asset=caos-x86_64-linux ;;
    Linux/aarch64|Linux/arm64) asset=caos-aarch64-linux ;;
    *)
        echo "no published binary for $(uname -s)/$(uname -m);" >&2
        echo "build from source with \`nix build .#caos-cli\`." >&2
        exit 1
        ;;
esac

# Track a BRANCH: install whatever that branch built most recently. GitHub has
# no "latest release matching a prefix", so this lists releases and picks one.
# The release workflow names a branch build `<branch>-<short sha>`, and that
# naming is the whole mechanism -- the suffix must be pure hex, or branch `cc`
# would claim `cc-conversations-<sha>` as its own.
#
# An explicit version wins: pinning is the stronger statement.
if [ -n "$BRANCH" ] && [ "$VERSION" = latest ]; then
    if ! command -v jq >/dev/null 2>&1; then
        echo "--branch needs jq (there is no prefix query in the releases API)" >&2
        exit 1
    fi
    # Not `curl | jq`: under `pipefail` a 403 from the API would surface as a
    # jq parse error, and rate limiting is the likeliest failure here.
    if ! releases="$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=100")"; then
        echo "could not list releases of $REPO (rate limited? repo private?)" >&2
        exit 1
    fi
    # Newest by publication, not by position: only the first 100 are fetched,
    # which is the right 100 because the API returns them newest first.
    VERSION="$(printf '%s' "$releases" | jq -r --arg b "$BRANCH" '
        [ .[] | select(.tag_name | startswith($b + "-"))
              | select(.tag_name[($b|length)+1:] | test("^[0-9a-f]{7,40}$")) ]
        | sort_by(.published_at) | last | .tag_name // empty')"
    if [ -z "$VERSION" ]; then
        echo "no release built from branch $BRANCH in $REPO" >&2
        echo "  (a branch build publishes as <branch>-<short sha>; has it run?)" >&2
        exit 1
    fi
    echo "branch $BRANCH -> $VERSION" >&2
fi

base="https://github.com/$REPO/releases"
if [ "$VERSION" = latest ]; then
    url="$base/latest/download/$asset"
else
    url="$base/download/$VERSION/$asset"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "fetching $url" >&2
curl -fsSL "$url" -o "$tmp/caos"
chmod +x "$tmp/caos"

# The published binary is the STATIC one, without the version wrapper nix adds,
# so on its own it reports an empty rev. A small wrapper puts the release back:
# telling a stale client from a current one is the only reason it prints at all.
# The release workflow stamps the tag over the placeholder below. Unstamped
# means this script came from the repo or raw from a branch -- and then the
# resolved release is a better answer than "dev", which would otherwise be what
# a branch-tracking install reports forever.
stamped="@CAOS_RELEASE@"
case "$stamped" in
    @CAOS_*)
        if [ "$VERSION" = latest ]; then stamped="dev"; else stamped="$VERSION"; fi
        ;;
esac
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
