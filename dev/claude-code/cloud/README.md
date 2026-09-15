# Running a caos session in a cloud container

A cloud session (claude.ai/code) needs two things: the **client** (`caos cc
hook` records the conversation, `caos cc serve` is the tool server — one
downloaded binary), and a **caos server** to point it at. Neither requires the
repository to carry anything.

The caos server is reached over an **iroh tunnel**: a `dumbpipe` listener on the
machine that runs the server, dialled from inside the container by node id. The
container only ever connects OUT — nothing listens for an inbound shell, which
is what the sandbox refuses. See `../dumbpipe-system-certs.patch` for why the
tunnel is our own build of dumbpipe rather than n0's.

## Nothing is committed to a repository

Configuration is user-level in the container, so one environment serves every
repo. Three routes were possible; only one works:

| source | |
|---|---|
| repo `.claude/settings.json` | works, but is a file in every repository |
| managed settings | ruled out — an Anthropic-hosted session "doesn't read a device's MDM profile or file" |
| **user-level settings written by the setup script** | **what this uses** |

Measured: a setup script that wrote `settings.json` into every candidate home
found the hooks firing from `/root`. The CLI runs as root, even though the repo
sits at `/home/user/repo` and Claude's own state under `/home/claude/.claude`.
All three are written anyway; it costs nothing.

`SessionStart` is what makes it repo-independent. The client finds caos through
a `caos` git remote and an arbitrary checkout has none, so the hook brings up
the tunnel and adds the remote per session, from user-level settings rather than
anything committed.

## Configuring the environment

**Setup script**: paste these two lines into the environment's "Setup script"
field (swap `main` for a branch or commit to test a change — everything else is
read back out of it):

```
B=https://raw.githubusercontent.com/Metta-AI/caos/main/dev/claude-code
curl -fsSL "$B/cloud/setup.sh" | bash -s -- --base="$B"
```

**Environment variables**: one of —
- `CAOS_IROH_TICKET` — the iroh ticket of the caos server's dumbpipe listener.
  The tunnel comes up on `127.0.0.1:19090` and the `caos` remote points there.
- `CAOS_SERVER_URL` — a direct URL, when the server is reachable without a
  tunnel.

**Network access**: the environment's normal egress is enough. GitHub (the
client), `api.anthropic.com`, and iroh's relays (`*.relay.n0.iroh.link`,
`dns.iroh.link`) are all reachable, and iroh needs the relays to connect the
tunnel. Measured from a container: all return 200.

## The ticket must name the RUNNING listener

This is the one failure that looks like something else. `CAOS_IROH_TICKET`
carries an iroh NODE id, derived from the listener's `IROH_SECRET`. Restart the
listener with a different secret and the node changes; the env's ticket then
names a node that no longer exists, and every session's `connect-tcp` dials a
dead node. Iroh's relay swallows the connection rather than refusing it, so it
presents as `caos_status: cannot reach the CAOS server … timeout` and a hanging
`git ls-remote caos` — indistinguishable from a network fault, which it is not.

Keep the listener's `IROH_SECRET` FIXED and the ticket is permanent — set the
env once and it survives restarts and laptop sleep. To check agreement without
starting anything: `IROH_SECRET=<the listener's> dumbpipe generate-ticket`
prints the node ticket; its prefix must match the env's `CAOS_IROH_TICKET`.

## What is proven

Measured end to end, a fresh session on a correctly-ticketed environment:

- **hooks fire** — `SessionStart` brings up the tunnel and adds the `caos`
  remote; `UserPromptSubmit`/`Stop`/`PreToolUse` record the conversation.
- **the MCP tool server is picked up** — the declaration in `/root/.claude.json`
  is honoured, `caos cc serve` is spawned, and the model can call it.
- **the full tool set resolves** — `bash read ls grep edit write caos-build
  caos-test caos-test-result log show diff merge spawn_agent run_async
  wait_agent harvest_agent`, within SECONDS of init when the server has the
  `std/llm-step` image cached warm. The `caos_status` placeholder stands in only
  until they arrive (`../../rust/crates/caos-cli/CC.md`).

The remaining latency is Anthropic's ~2 minutes of provisioning and init, which
is fixed on their side; the first session against a step-tree the server has
never built also waits out one rustc compile, and only that first one.

## The setup-time budget

The setup script is asked to finish in roughly five minutes so the filesystem
snapshot can be taken, and a cold `nix build` of this tree will not fit. That
matters more than a slow first run: work done AFTER the snapshot is never cached,
so a build deferred into a hook is paid again by **every** session.

`setup.sh` therefore only DOWNLOADS the client (a static binary from the
release), and the per-session hook re-runs the installer — which stops at one
`ls-remote` when the build is already current. Nothing compiles in the
container.

## A separate, untested direction: the stack in the container

Everything above points the session at an EXTERNAL caos server over the tunnel.
Running the caos stack INSIDE the cloud container is a different thing and is not
done here — the VM has `docker`/`dockerd`, but caos runs containers that run
containers, and whether nested/privileged workloads are permitted is unmeasured.
Only pursue it if the external-server model proves insufficient.
