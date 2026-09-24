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

These four scripts are due to be replaced by one Go program run from the setup
field; [`design/cloud-setup.md`](../../../design/cloud-setup.md) has the plan and
the measurements that motivate it, including the three assumptions in this
document that turned out to be false.

## The session starts from a CLIENT repo, not from the code

The repository a session opens is a **caos client repo** — a handful of text
files that pin a caos and mount its `std/`. `Metta-AI/caos-session` is the one
to fork; its shape is spelled out under *Configuring the environment* below,
so this does not depend on being able to open it. The code to work on is
**imported into the conversation** by the agent's `import_source` tool — the
server fetches it from GitHub directly, so nothing is cloned into the container
and nothing is pushed out of it.

That is what makes starting a session cheap on a large repository, and the
mechanism is the whole of the reason. The old arrangement checked the TARGET
repo out shallow, then had to unshallow it in the session hook — `caos` pushes
the workspace commit and a push packs its whole reachable graph, so the history
had to be present — and then pushed that graph to the server. All of it scaled
with the repository, and all of it landed before the first turn. Now the
server fetches the repository itself, once, and the container's checkout is the
client repo.

Measured on a client repo: the session hook reaches `unshallow done` at
**+3s**, refresh included. The unshallow is kept rather than deleted — a fork
that accumulates history still needs it, and at this size it costs nothing.
What a large TARGET repository costs was not measured before the change and is
not claimed here; what changed is that it is no longer on this path at all.

The client repo is also the **version knob**. `setup.sh` reads its `flake.lock`
before installing anything, so the client binary, the tools and the tree the
session evaluates all come from the commit the repo pins — and
`session-start.sh` re-reads it every session, because the environment snapshot
freezes whatever setup resolved. `caos-pin.sh` is that reader, used by both.

With caos reachable at a path in the checkout (`caos-std/`, from the repo's
root `.caos-expr`), the step is named as one: `--llm-step:@=caos-std/llm-step`
rather than a locator pinned to the client's own build. The same path makes
`reader=caos-std/llm-step` resolvable in a **committed** `.caos-secrets` entry,
which is how a shared repo can declare a GitHub token without holding one.

Claude Code's own configuration is still user-level in the container, so one
environment serves every repo. Three routes were possible; only one works:

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

**The checkout is there before the setup script runs**, which is what lets the
pin be read that early — and reading it early matters, because work done after
the snapshot is paid by every session. Measured from a session's
`env_manager_log`: `Cloned from seed bundle` precedes `Running setup script`.

A repo that pins no caos is a misconfigured environment and setup **fails**,
naming what is missing. It does not fall back to the `--base` in the settings
form: that names a branch, and a client installed from a moving head would be a
client from a different tree than the tools it drives.

## Configuring the environment

**Repository**: a caos client repo — fork `Metta-AI/caos-session` and point the
environment at your fork. It is four things, and nothing else:

```
flake.nix / flake.lock   a `caos` input pinned by revision — the version knob
.caos-expr               one line, mounting that input's std/ at --output-path
AGENTS.md (+ CLAUDE.md)  what the agent is told at the start of every session
.caos-secrets/           secret DECLARATIONS — names and readers, no values
.gitignore               the mount point, which must not exist as a real directory
```

The root expression is the `std/flake-input-loader` line from
[`design/flake-inputs.md`](../../../design/flake-inputs.md) ("Consumer root"),
on **one** line — `eval` splits on lines, so a trailing `\` is a token rather
than a continuation.

**Setup script**: paste these two lines into the environment's "Setup script"
field (swap `main` for a branch or commit to test a change to the *bootstrap
scripts* — everything else is read back out of it):

```
B=https://raw.githubusercontent.com/Metta-AI/caos/main
curl -fsSL "$B/integrations/claude-code/cloud/setup.sh" | bash -s -- --base="$B"
```

This `--base` says where `setup.sh`, `caos-pin.sh` and `session-start.sh` come
from, and **only** that. It does not choose the caos that gets installed — the
repository's `flake.lock` does, and `install.sh` accepts nothing but a full
commit sha, so the branch here can never become the client. A repository that
pins no caos does not fall back to this branch; setup fails, naming what is
missing. The line above then never needs editing
again.

**Environment variable**: `CAOS_SERVER_URL`, holding either —
- `caos://<ticket>` — what `caosd ticket` prints on the machine running the
  server (brought up with `caosd up --iroh`). Reachable from anywhere.
- a plain `http://…` URL, when the server is reachable without one.

`CAOS_SERVER_URL` is a **credential** when it is a ticket: whoever holds it can
drive that server, which runs containers and holds every secret. The status tool
redacts it rather than printing it for a model to quote.

**For private repositories**, two more, both named by the client repo's
committed `.caos-secrets/github-token` rather than by anything here:

- `GITHUB_TOKEN` — a PAT with access to the repos you want to import or publish.
- `CAOS_GITHUB_TOKEN_ENTROPY` — 16+ random characters, yours alone. This is
  what keeps your cached results unreadable by anyone else holding the same
  client repo; it is a bearer capability for the cache, which is why it comes
  from the environment and never from the committed file.

Leave both unset for public work: the secret is simply absent from the store
(one line on stderr), and imports of public repositories need no credential.

One thing to know before setting only one of them: **the container already has
a `GITHUB_TOKEN`**, put there by the harness for its own clone. Measured — a
session with neither variable set warned about the ENTROPY, and that warning is
only reached once the value has resolved. So setting just
`CAOS_GITHUB_TOKEN_ENTROPY` promotes the harness's token into a caos secret,
which may be what you want or may not. Requiring both is what keeps that a
decision: an ambient token with no entropy would otherwise have run with no
cache isolation at all.

**Network access**: the environment's normal egress is enough. GitHub (the
client), `api.anthropic.com`, and iroh's relays are reachable from a SESSION;
the setup phase is a different path and refuses the n0 relays (see *Dev mode*
below, which is the only thing that needs them that early).

One thing the container needs that a laptop does not: the **OS trust store**.
Egress here is a TLS-intercepting proxy, so the relay presents the proxy's
certificate, which chains to a CA only the system knows about — a build using
iroh's compiled-in Mozilla roots reaches NO relay while curl and git on the same
host are fine. `caos_iroh::endpoint_builder` asks for the system store and
honours `HTTPS_PROXY`, and the release checks the built helper for the markers
that prove it.

## Dev mode: the whole install package from your own caosd

A third line on the setup command runs the session against the caos on your
machine, with no push and no CI:

```
B=https://raw.githubusercontent.com/Metta-AI/caos/main
curl -fsSL "$B/integrations/claude-code/cloud/setup.sh" | bash -s -- --base="$B" \
  --dev-server=caos://<ticket>
```

`caosd up --iroh` publishes the working checkout to `refs/caos/dev` on that
server: one commit carrying the tree and the x86_64 binaries built from it.
`setup.sh` fetches that commit and takes **everything** from it — the client,
`git-remote-caos`, `settings.json`, `mcp.json`, the installer, and this script
itself (re-exec'd once from the dev tree, so an edit here is in the package like
anything else). It then repoints the checkout's `.caos-expr` and `flake.lock` at
`git+caos://…?rev=<dev>`, so the tools resolve from there too.

It is an **argument, not an environment variable**: the setup phase does not get
the environment's variables. Measured — a session stamped `off` while the
environment plainly set `CAOS_DEV=1`, which is why that variable is gone.

Two things are less obvious and both cost a session to find:

- **The committed GitHub pin is still required**, as a bootstrap. Fetching over
  `caos://` needs `git-remote-caos`, and the release install is what puts it on
  PATH; the dev package then replaces everything it laid down.
- **The conversation seeds from an unreferenced commit**, minted by
  `commit-tree` from the rewritten checkout and passed to the hook as
  `--base=<full sha>`. The hook — not `mcp serve` — is what creates a
  conversation, and without this it would seed from `HEAD`: the session would
  run your client while evaluating the *committed* tools, the half-update the
  whole arrangement exists to prevent. Nothing points at that commit, so a
  `git push` cannot carry the ticket it contains.

`/usr/local/share/caos/dev-stamp` is written last, once every step has
succeeded; `caos_status` and the session hook both report from it, and each
step is fatal, so the file existing is the claim.

**The relay has to be one of yours.** The setup phase reaches the network
through a TLS-terminating gateway that answers **503** for all seven n0 relays
(measured; controls on the same network pass), while the session hook, which
egresses through a local `CONNECT` proxy, reaches them fine. A dev stack's
ticket carries only private addresses, so the relay is the only path — run
`iroh-relay --dev` on a host of your own and point `CAOS_IROH_RELAY` at it
(`prod/caosd/configuration.nix` has a unit for it). **Port 80 or 443 only**:
both phases carry standard ports and time out on anything else.

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

Measured again on a client repo, against a server that already held the std
entries: resolving `--llm-step:@=caos-std/llm-step` through the
`flake-input-loader` mount took **15.0s** and cached **20 tools** on the first
attempt. Moving the pin does NOT by itself force a rebuild — `std/llm-step`'s
tree is content-addressed, so a caos commit that does not touch it resolves to
the same oid and stays a memo hit.

**A session that records the prompt and then never takes a turn is a step that
will not resolve.** `UserPromptSubmit` cannot form a request without resolving
`--llm-step`, so a broken expression presents as a silent session rather than
an error: the transcript holds one user event, no assistant event, and the hook
log stops after `warming the caos tool registry`. Run
`caos mcp warm "--llm-step:@=<path>/llm-step"` in a checkout with the `caos`
remote set — it prints the real reason, which the session never does.

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
