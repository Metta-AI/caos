# Running a caos session in a cloud container

A cloud session (claude.ai/code) needs two things: the **client** (`caos mcp
hook` records the conversation, `caos mcp serve` is the tool server — one
downloaded binary), and a **caos server** to point it at. Neither requires the
repository to carry anything.

The caos server is named by a **ticket**: `CAOS_SERVER_URL=caos://<ticket>`, and
the client speaks that transport itself (`design/iroh-transport.md`). The
container only ever connects OUT — nothing listens for an inbound shell, which
is what the sandbox refuses.

There is no tunnel process and no local port. This used to be a `dumbpipe`
connector the session hook started on `127.0.0.1:19090`, with a liveness poll, a
`pkill` for a stale one holding the port, and `setsid` to survive Claude Code's
teardown of the hook's process group; all of that is gone. What replaced it is
smaller AND faster: dumbpipe dials the far endpoint once per accepted socket, so
every request paid a fresh connection — the reason `/eval-locator` exists — while
the client now holds ONE connection and opens a stream per request.

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
a `caos` git remote and an arbitrary checkout has none, so the hook adds it per
session, from user-level settings rather than anything committed.

## Configuring the environment

**Setup script**: paste these two lines into the environment's "Setup script"
field (swap `main` for a branch or commit to test a change — everything else is
read back out of it):

```
B=https://raw.githubusercontent.com/Metta-AI/caos/main
curl -fsSL "$B/integrations/claude-code/cloud/setup.sh" | bash -s -- --base="$B"
```

**Environment variable**: `CAOS_SERVER_URL`, holding either —
- `caos://<ticket>` — what `caosd ticket` prints on the machine running the
  server (brought up with `caosd up --iroh`). Reachable from anywhere.
- a plain `http://…` URL, when the server is reachable without one.

`CAOS_SERVER_URL` is a **credential** when it is a ticket: whoever holds it can
drive that server, which runs containers and holds every secret. The status tool
redacts it rather than printing it for a model to quote.

**Network access**: the environment's normal egress is enough. GitHub (the
client), `api.anthropic.com`, and iroh's relays (`*.relay.n0.iroh.link`,
`dns.iroh.link`) are all reachable. Measured from a container: all return 200.

One thing the container needs that a laptop does not: the **OS trust store**.
Egress here is a TLS-intercepting proxy, so the relay presents the proxy's
certificate, which chains to a CA only the system knows about — a build using
iroh's compiled-in Mozilla roots reaches NO relay while curl and git on the same
host are fine. `caos_iroh::endpoint_builder` asks for the system store and
honours `HTTPS_PROXY`, and the release checks the built helper for the markers
that prove it.

## The ticket keeps working; a re-keyed server does not

A ticket carries the server's endpoint id and its token, both persisted under
`$CAOS_DATA/stack/iroh/`. Restart, reboot, sleep — the same ticket keeps
working, which is why one lives in the environment indefinitely.

What invalidates it is `caosd reset`, which wipes that directory and mints a new
identity. The env's ticket then names an endpoint nobody answers for, and iroh's
relay swallows the connection rather than refusing it — so it presents as
`caos_status: cannot reach the CAOS server … timeout` and a hanging
`git ls-remote caos`, indistinguishable from a network fault, which it is not.
`caosd ticket` on the server prints the current one; its prefix must match the
environment's.

## What is proven

Measured end to end, a fresh session on a correctly-ticketed environment:

- **hooks fire** — `SessionStart` adds the `caos` remote;
  `UserPromptSubmit`/`Stop`/`PreToolUse` record the conversation.
- **the MCP tool server is picked up** — the declaration in `/root/.claude.json`
  is honoured, `caos mcp serve` is spawned, and the model can call it.
- **the full tool set resolves** — `bash read ls grep edit write caos-build
  caos-test caos-test-result log show diff merge spawn_agent run_async
  wait_agent harvest_agent`, within SECONDS of init when the server has the
  `std/llm-step` image cached warm. The `caos_status` placeholder stands in only
  until they arrive (`../../rust/crates/caos-cli/MCP.md`).

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

Everything above points the session at an EXTERNAL caos server by ticket.
Running the caos stack INSIDE the cloud container is a different thing and is not
done here — the VM has `docker`/`dockerd`, but caos runs containers that run
containers, and whether nested/privileged workloads are permitted is unmeasured.
Only pursue it if the external-server model proves insufficient.
