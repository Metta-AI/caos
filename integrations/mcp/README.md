# mcp — the caos MCP server

caos exposes its workspace as an MCP server, so any MCP client — Codex, Claude
Code, an editor's agent — can drive caos tools instead of its own built-ins.

There is no deployable here: the server **is** the client binary. `caos mcp serve`
is a stdio JSON-RPC MCP server (rust/crates/caos-cli/src/mcp/, documented in that
crate's `MCP.md`). It offers five fixed tools — `run`, `help`, `start`, `result` and
`eval-path` — and what they run is named by a path in the conversation (`caos-std/read`,
`caos-std/bash-tool`, ...), each call a caos run recorded into a
conversation. `caos mcp hook` records the session.

What a PARTICULAR client needs to launch, configure and record against it — its
config format, its hooks, its cloud packaging — lives under that client's name
(see [`../claude-code`](../claude-code)). Pointing a new client at `caos mcp
serve` is the whole integration.
