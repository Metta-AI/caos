# integrations

Where caos meets an external agent or automation harness. Each subdirectory is
one harness, and holds the host-side glue that wires it to a caos server — in
either direction:

- **caos exposed TO a harness** — caos as the tool provider the harness drives.
  For Claude Code that is an MCP tool server plus the hooks that record the
  session, so the harness runs caos workspace tools instead of its built-ins.
- **caos DRIVING a harness** — caos provisioning and steering the harness. For
  Claude Code that is the cloud-session setup: install the client, bring up the
  tunnel to the caos server, warm the tools, and drive sessions.

This is deliberately NOT under `dev/` (caos's own development tooling) and NOT at
the top level loose: it is its own category, because there will be more of it —
another coding agent, or a CI system driving caos — each a sibling here.

What lives here is the deployable glue: install scripts, session hooks, the
harness's configuration, and any patched tools it needs. The caos-side
counterpart to a harness integration may be a verb in the client itself — Claude
Code's is `caos cc` (`rust/crates/caos-cli/src/cc/`, documented in that crate's
`CC.md`) — which stays compiled into the binary; only the host-side package lives
here.

## Harnesses

- [`claude-code/`](claude-code/) — Anthropic's Claude Code, both as an MCP tool
  server over caos and as cloud sessions caos provisions and drives from a
  single `--base` URL.
