#!/bin/bash
# Install the caos Claude Code client into a repository.
#
#   curl -fsSL <release-url>/install.sh | bash
#   curl -fsSL <release-url>/install.sh | bash -s -- --force
#
# WHICH BUILD, in increasing order of precedence:
#
#   (nothing)     the newest build on main
#   --branch=X    the newest build on branch X (a sha or tag works too)
#   --commit=C    the build of commit C (short sha, full, any ref)
#   --version=T   release T, named outright, no API call
#
# Resolving a branch or commit needs `jq` and two anonymous GitHub API calls.
# `--version` needs neither, and is the way out if either is unavailable.
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

# Arguments only. Which build to install is not read from the environment: it
# is the kind of setting that gets exported once and then silently outranks the
# argument someone is looking straight at.
REPO="Metta-AI/caos"
VERSION=""
COMMIT=""
# The default is a branch like any other, so the ordinary case and the pinned
# case go down the same path and only one of them can be quietly broken.
BRANCH="main"
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
        --commit=*) COMMIT="${arg#--commit=}" ;;
        --repo=*)   REPO="${arg#--repo=}" ;;
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

# A build is named by its COMMIT -- `build-<12 hex>` -- and by nothing else, so
# resolving one is a lookup rather than a parse. It was `<branch>-<sha>` once,
# and since `-` is legal in a branch name, `cc` and `cc-conversations` made tags
# no rule could separate; asking GitHub what is on a branch is the exact
# question that name was a lossy encoding of.
#
# COMMIT wins over BRANCH, and an explicit VERSION wins over both: each is a
# stronger statement than the one under it.
gh_api() { # <path> -- fails loudly, because rate limiting is the likely one
    if ! curl -fsSL "https://api.github.com/repos/$REPO/$1"; then
        echo "GitHub API: $REPO/$1 (rate limited? private? no such ref?)" >&2
        return 1
    fi
}

if [ -z "$VERSION" ]; then
    if ! command -v jq >/dev/null 2>&1; then
        echo "resolving a branch or commit needs jq" >&2
        echo "  (or name a release outright: --version=<tag>)" >&2
        exit 1
    fi

    if [ -n "$COMMIT" ]; then
        # Through the API rather than used as given: this accepts a short sha,
        # a full one, or anything else that names a commit, and returns the one
        # canonical form the tag was built from.
        # A 422 here means GitHub has never seen the commit, and the ordinary
        # cause is that it exists only locally. Say so: "no such commit" about
        # a sha sitting in `git log` reads as a bug in this script.
        if ! sha="$(gh_api "commits/$COMMIT" | jq -r '.sha // empty')" || [ -z "$sha" ]; then
            echo "$REPO does not have commit $COMMIT -- is it pushed?" >&2
            exit 1
        fi
        VERSION="build-$(printf '%s' "$sha" | cut -c1-12)"
        echo "commit $COMMIT -> $sha -> $VERSION" >&2
    else
        # The newest commit ON THE BRANCH that actually has a build. Not simply
        # the branch head: a push and a session start seconds apart would find
        # the workflow still running, and the previous build is a far better
        # answer than a failure. Two calls, then a set intersection.
        have="$(gh_api "releases?per_page=100" | jq -r '.[].tag_name')"
        commits="$(gh_api "commits?sha=$BRANCH&per_page=30" | jq -r '.[].sha')"
        VERSION=""
        while IFS= read -r sha; do
            candidate="build-$(printf '%s' "$sha" | cut -c1-12)"
            if printf '%s\n' "$have" | grep -qxF "$candidate"; then
                VERSION="$candidate"
                break
            fi
        done <<< "$commits"
        if [ -z "$VERSION" ]; then
            echo "no build published for any of the last 30 commits on $BRANCH" >&2
            echo '  (the workflow publishes build-<commit>; has it run?)' >&2
            exit 1
        fi
        echo "branch $BRANCH -> $VERSION" >&2
    fi
fi

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
# so on its own it reports an empty rev. A small wrapper puts the release back:
# telling a stale client from a current one is the only reason it prints at all.
# The release workflow stamps the tag over the placeholder below. Unstamped
# means this script came from the repo or raw from a branch -- and then the
# resolved release is a better answer than "dev", which would otherwise be what
# a branch-tracking install reports forever.
stamped="@CAOS_RELEASE@"
case "$stamped" in
    @CAOS_*) stamped="$VERSION" ;;
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
