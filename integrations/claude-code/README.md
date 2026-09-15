# claude-code

Anthropic's Claude Code, wired to caos — in both directions, sharing one config:

- **caos AS Claude Code's tools**: the `caos mcp serve` MCP server (see
  [`../mcp`](../mcp)), declared in `shared/mcp.json`, with Claude Code's built-in
  tools denied in `shared/settings.json` so the model uses caos's instead.
- **caos DRIVING Claude Code**: cloud sessions caos provisions and steers.

```
shared/   the deny list, hooks, and server declaration both paths consume
cli/      launch Claude Code locally against caos (run, remote-control)
cloud/    configure a claude.ai/code cloud environment
            install.sh        the installer: binary + user-level config, pinned
            setup.sh          the env's "Setup script" field: install once, wire the hook
            session-start.sh  per-session: remote, tunnel, refresh, unshallow, warm
            drive             drive a cloud session from a terminal
            dumbpipe-system-certs.patch   the iroh tunnel, patched for the OS trust store
```

The rust side — `caos mcp` — is compiled into the client (`rust/crates/caos-cli/`,
see its `MCP.md`); only this host-side glue lives here.
