# TUI backend boundary

`caos tui` is a first-party terminal client for the same conversation engine as
`caos chat` and `caos talk`. It does not introduce a second agent loop or a wire
protocol. The `caos-cli` binary owns the terminal lifecycle and rendering,
while the `caos` library remains responsible for conversation history, tools,
model turns, and durable Git state.

The boundary between them borrows a few proven ideas from Agent Client Protocol
(ACP) without depending on ACP:

- Capabilities are explicit. The client does not render an operation as
  available unless its backend advertises it. Cancellation is currently false.
- Conversations are loaded as complete snapshots: ordered messages plus their
  accumulated workspace diff.
- A running turn produces typed updates for status, assistant text, tool calls,
  tool results, phase timing, and completion. Rendering never parses log text to
  infer state.
- Tool calls have stable IDs so a result updates the activity that initiated it,
  even when conversations run in the background.

These types and the `ChatBackend` trait live in
`crates/caos/src/bin/tui/backend.rs`.
`CaosBackend` is the production adapter to `caos::chat`; tests use an in-memory
backend to exercise the UI without a server or checkout. This keeps the seam
small enough to replace or extend later while avoiding JSON-RPC, an async
runtime, or a third-party TUI in the build closure.

The backend and terminal client are compiled directly into the host
`caos-cli` binary. `caos tui` invokes the client in-process.
