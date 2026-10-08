# claude-code

Anthropic's Claude Code, wired to caos — in both directions, sharing one config:

- **caos AS Claude Code's tools**: the `caos mcp serve` MCP server (see
  [`../mcp`](../mcp)), declared in `shared/mcp.json`, with Claude Code's built-in
  tools denied in `shared/settings.json` so the model uses caos's instead.
- **caos DRIVING Claude Code**: cloud sessions caos provisions and steers.

```
shared/   the deny list, hooks, and server declaration both paths consume
cli/      launch Claude Code locally against caos (run)
drive/    start and inspect cloud sessions, from a terminal or as a caos worker
            drive.go          the program; .caos-expr + DEPS make it an entry
cloud/    configure a claude.ai/code cloud environment
            bootstrap.go      stage 1: read the repo's pin, fetch the payload, run stage 2
            install.go        stage 2: put the package in place, write Claude Code's config
            session.go        the SessionStart hook: warm the tool registry
```

The deny list also bans the GitHub MCP server's tools (`mcp__github__*`) that
caos covers with `import_source`, `publish_source` and `caos-std/github` (see
[`std/GIT.md`](../../std/GIT.md)). Six stay available, because `caos-std/github`
cannot do what they do or is not shown to: `get_job_logs` (GitHub serves CI logs
through a redirect `caos-std/github` does not follow), `run_secret_scanning`, and
the four Copilot tools (`assign_copilot_to_issue`,
`create_pull_request_with_copilot`, `request_copilot_review`,
`get_copilot_job_status`). A tool added to the GitHub MCP server later is not
banned until it is listed.

The rust side — `caos mcp` — is compiled into the client (`rust/crates/caos-cli/`,
see its `MCP.md`); only this host-side glue lives here.
