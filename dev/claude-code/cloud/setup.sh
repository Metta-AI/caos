#!/bin/bash
# Cloud-environment SETUP SCRIPT for a caos session (claude.ai/code).
#
# Paste into the environment's "Setup script" field. Runs ONCE, as root on
# Ubuntu 24.04, before Claude Code launches; the filesystem is then snapshotted
# and later sessions start from it with this skipped. What survives is what is
# written to DISK -- packages, binaries, docker images. Anything merely RUNNING
# does not, which is why nothing is started here.
#
# THE CLIENT IS A DOWNLOAD, not a build. The five-minute budget is only half the
# reason: work done after the snapshot is never cached, so compiling here would
# be paid again by every session that misses. GitHub is already on the Trusted
# allowlist, so this needs no network-policy change.
set -uo pipefail
export DEBIAN_FRONTEND=noninteractive

# ---------------------------------------------------------------------------
# The client -- the only permanent part
# ---------------------------------------------------------------------------

# `install.sh` also writes .claude/settings.json and .mcp.json into the
# repository, and leaves existing ones alone unless --force. A freshly cloned
# cloud checkout has nothing to preserve, so --force is right here and would not
# be on a developer's machine.
repo="${CAOS_REPO:-Metta-AI/caos}"
if [ -n "${CAOS_VERSION:-}" ]; then
    installer="https://github.com/$repo/releases/download/$CAOS_VERSION/install.sh"
else
    installer="https://github.com/$repo/releases/latest/download/install.sh"
fi
curl -fsSL "$installer" | bash -s -- --force || true

# ---------------------------------------------------------------------------
# The stack, while it still runs in this VM
# ---------------------------------------------------------------------------
# EXPERIMENT ONLY, and off unless asked for. Delete all of this once there is a
# caos server to point at: a session needs the client and a URL, nothing else.
# Nix and docker are here solely to run caos itself inside the container.

if [ -z "${CAOS_IN_VM_STACK:-}" ]; then
    exit 0
fi

if [ ! -x /nix/var/nix/profiles/default/bin/nix ]; then
    curl -L https://nixos.org/nix/install -o /tmp/nix-install.sh \
        && sh /tmp/nix-install.sh --no-daemon --yes || true
fi
mkdir -p /etc/nix
printf 'experimental-features = nix-command flakes\nmax-jobs = auto\n' > /etc/nix/nix.conf
if [ -e /nix/var/nix/profiles/default/etc/profile.d/nix.sh ]; then
    ln -sf /nix/var/nix/profiles/default/etc/profile.d/nix.sh /etc/profile.d/nix.sh
    printf 'export PATH=/nix/var/nix/profiles/default/bin:$PATH\n' >> /etc/bash.bashrc
fi

# Fetch rather than build: a cold `nix build` of this tree does not fit the
# budget, and what is built after the snapshot is never cached.
export PATH=/nix/var/nix/profiles/default/bin:$PATH
if [ -f flake.nix ]; then
    timeout 200 nix flake archive --no-write-lock-file . >/dev/null 2>&1 || true
fi

# Pull base images so the snapshot carries them; dockerd itself is started per
# session by the SessionStart hook, since a daemon started here is not in it.
(dockerd >/tmp/dockerd-setup.log 2>&1 &) || true
for _ in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker pull redis:7-alpine >/dev/null 2>&1 || true
docker pull registry:2 >/dev/null 2>&1 || true

exit 0
