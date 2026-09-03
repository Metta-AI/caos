# Running a caos session in a cloud container

Two separable things. The **client** is what a cloud session needs permanently:
`caos cc hook` records the conversation and `caos cc serve` is the tool server,
and both are one downloaded binary. The **stack** running inside the VM is a
short-term experiment so there is a caos server at all — it lives and dies with
the session, needs no hosting and no authentication work, and is off unless
`CAOS_IN_VM_STACK` is set.

## The two pieces, and why they are two

| | runs | where it is configured |
|---|---|---|
| `setup.sh` | ONCE, before Claude Code launches, then the filesystem is snapshotted and it is skipped | the environment's **Setup script** field at claude.ai/code |
| `session-start.sh` | EVERY session, cloud or local, including resumed | the repo's `.claude/settings.json`, as a `SessionStart` hook |

The split is forced by what the cache keeps. It is a filesystem snapshot:
packages, docker images and `/nix` carry over; anything merely *running* does
not. So `caosd up` cannot go in the setup script — a stack started there is not
in the snapshot — and installing nix cannot go in the hook, or every session
would pay for it.

## Configuring the environment

**Setup script**: paste `setup.sh`. By default it does one thing — fetch the
release and run its `install.sh`, which drops `caos` on `PATH` and writes
`.claude/settings.json` and `.mcp.json` into the checkout. Seconds, and it lands
in the snapshot.

**Network access**: the default **Trusted** level already covers GitHub, so the
client needs no change at all. Only the in-VM stack experiment needs **Custom**,
adding what nix fetches from:

```text
nixos.org
*.nixos.org
cache.nixos.org
```

**To run the stack in the VM too** (the experiment), set `CAOS_IN_VM_STACK=1` in
the environment's variables and add the SessionStart hook to the repo's
`.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command",
                     "command": "dev/claude-code/cloud/session-start.sh" } ] }
    ]
  }
}
```

Without `CAOS_IN_VM_STACK` the setup script installs the client and stops, which
is the shape once caos is hosted: point `CAOS_SERVER_URL` at the server and
nothing else changes.

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
