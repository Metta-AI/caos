# claude-code

Anthropic's Claude Code, wired to caos — in both directions, sharing one config:

- **caos AS Claude Code's tools**: the `caos mcp serve` MCP server (see
  [`../mcp`](../mcp)), declared in `shared/mcp.json`, with Claude Code's built-in
  tools denied in `shared/settings.json` so the model uses caos's instead.
- **caos DRIVING Claude Code**: cloud sessions caos provisions and steers.

```
shared/   the deny list, hooks, and server declaration both paths consume
cli/      launch Claude Code locally against caos (run)
cloud/    configure a claude.ai/code cloud environment
            bootstrap.go      stage 1: read the repo's pin, fetch the payload, run stage 2
            install.go        stage 2: put the package in place, write Claude Code's config
            session.go        the SessionStart hook: warm the tool registry
            drive.go          start and inspect cloud sessions from a terminal
```

The rust side — `caos mcp` — is compiled into the client (`rust/crates/caos-cli/`,
see its `MCP.md`); only this host-side glue lives here.
