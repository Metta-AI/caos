# Running a caos session in a cloud container

A cloud session needs two things: the **client** (`caos cc hook` records the
conversation, `caos cc serve` is the tool server — one downloaded binary), and a
**caos server** to point it at. Neither requires the repository to carry
anything.

## Nothing is committed to a repository

Configuration is user-level in the container, so one environment serves every
repo. Three routes were possible; only one works:

| source | |
|---|---|
| repo `.claude/settings.json` | works, but is a file in every repository |
| managed settings | ruled out — an Anthropic-hosted session "doesn't read a device's MDM profile or file" |
| **user-level settings written by the setup script** | **what this uses** |

Measured, not assumed: a setup script that wrote `settings.json` into every
candidate home found the hooks firing from `/root`. The CLI runs as root, even
though the repo sits at `/home/user/repo` and Claude's own state under
`/home/claude/.claude`. All three are written anyway; it costs nothing.

`SessionStart` is what makes it repo-independent. The client finds caos through
a `caos` git remote and an arbitrary checkout has none, so the hook adds it from
`$CAOS_SERVER_URL` — per-repo configuration applied from user-level settings.

## Configuring the environment

**Setup script**: paste `setup.sh`. It downloads the client from the latest
release, writes the hooks and deny list, declares the tool server, and touches
no checkout.

**Environment variables**: `CAOS_SERVER_URL` for the caos server to use.
Optionally `CAOS_VERSION` to pin a release.

**Network access**: the default **Trusted** level covers GitHub, which is where
the client comes from. Reaching the caos server needs whatever that server's
transport needs.

## Still unproven

The tool server is declared in `/root/.claude.json`, and while hooks are
measured to be read from `/root/.claude/settings.json`, the MCP declaration
beside it is NOT yet confirmed to be picked up. It is the same mechanism the
remote-control launcher uses successfully, but that is reasoning rather than
measurement.

## The risk worth measuring first

The setup script is asked to finish in roughly five minutes so the cache can
build, and a cold `nix build` of this tree will not fit. That matters more than
a slow first run: work done AFTER the snapshot is never cached, so a build
deferred into the hook is paid again by **every** session.

`setup.sh` therefore fetches (`nix flake archive`) rather than builds, and the
hook builds in the background. Whether that is tolerable depends on how long a
build takes with a warm store and no compilation left to do — `time nix build`
from a cold store is the number to get before investing further.

If it is not tolerable, the fix is a binary cache (cachix or attic) that the
setup script substitutes from, turning the build into a download that fits in
the budget and lands in the snapshot.

## Unknowns to test, in order

1. **Do hooks fire at all in a cloud session?** Everything rests on this. The
   cheapest check is a `SessionStart` hook that writes a file, and a session that
   looks for it.
2. **Does dockerd run in the VM, and can it run the stack's containers?** The VM
   has `docker`/`dockerd` pre-installed, but caos runs containers that run
   containers, and whether nested/privileged workloads are permitted here is not
   documented either way.
3. **Does the stack come up at all**, and how long the first turn waits for it.

Only after those does any of the hosted-caos work become worth doing.
