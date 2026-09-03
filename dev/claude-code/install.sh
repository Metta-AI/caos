#!/bin/bash
# Install the caos Claude Code client into a repository.
#
#   curl -fsSL <release-url>/install.sh | bash
#   curl -fsSL <release-url>/install.sh | bash -s -- --force
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
# The release workflow stamps the tag over the placeholder below; run straight
# from the repo it stays "dev", which is the honest answer there.
stamped="@CAOS_RELEASE@"
case "$stamped" in
    @CAOS_*) stamped="dev" ;;
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
