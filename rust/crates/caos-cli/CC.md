# `caos cc` — Claude Code against a caos workspace

`caos cc` lets Claude Code drive a caos conversation. Claude Code runs the
model; caos keeps the durable log and owns the workspace. The result is an
ordinary conversation under `refs/caos/v2/conversations/cc/<session-id>/head`
— the same ref layout, the same append-only event spine, and the same replay
`caos tui` performs for a conversation it started itself (design/chat.md).

Two commands make that work:

```text
caos cc hook     record one Claude Code hook payload (JSON on stdin)
caos cc serve    the workspace tool server (JSON-RPC on stdio)
```

## What the model sees

Claude Code's built-in file and shell tools are denied, which removes them from
the model's context entirely rather than merely refusing their calls. In their
place the tool server registers four tools over the conversation's workspace:

| Tool | Arguments |
|---|---|
| `read` | `file_path`, optional `offset`/`limit` (line-based) |
| `ls` | optional `path` |
| `write` | `file_path`, `content` |
| `edit` | `file_path`, `old_string`, `new_string`, optional `replace_all` |

They are the host-side counterparts of the worker's inline tools
(`std/llm-step/src/tools.rs`) and behave the same way, including the read
truncation cap and `edit`'s must-match-exactly-once rule.

**The workspace is the conversation's tree, not your checkout.** A `write` never
touches a file on disk; it produces a new tree and appends an event carrying it.
Your working copy is untouched for the whole session, exactly as it is when the
TUI runs a turn. To bring the result into your checkout, use the conversation
like any other — `caos tui`, then `Ctrl+L`.

## Configuration

The tool server is an ordinary child process, so it is configured like any
stdio MCP server:

```json
{ "mcpServers": {
    "caos": { "type": "stdio", "command": "caos", "args": ["cc", "serve"] } } }
```

and the session is launched against it:

```bash
claude --mcp-config .mcp.json --strict-mcp-config
```

`.claude/settings.json` supplies the hooks and the deny list:

```json
{
  "permissions": {
    "deny": ["Read", "Write", "Edit", "Bash", "Glob", "Grep", "NotebookEdit"],
    "allow": ["mcp__caos"]
  },
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command",
                                       "command": "caos cc hook" }] }],
    "PreToolUse": [{ "matcher": "mcp__caos__.*",
                     "hooks": [{ "type": "command",
                                 "command": "caos cc hook" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "caos cc hook" }] }],
    "StopFailure": [{ "hooks": [{ "type": "command",
                                  "command": "caos cc hook" }] }]
  }
}
```

Every hook is the same command. A payload names its own event
(`hook_event_name`), so nothing here needs a wrapper script, `jq`, or any shell
quoting — which is deliberate, given what CLAUDE.md records about this tree's
shell.

`PreToolUse` is not optional. The tool server is stateless, and that hook is the
only thing that tells a tool which conversation it is working in: it adds
`caos_session` and `caos_tool_use_id` to the call through `updatedInput`. Both
are declared in every tool's schema, so a call remains schema-valid with or
without them, and the hook only fills in values the tool already accepted.
Without it, every tool call fails saying so.

The hook emits no `permissionDecision`. Whether these tools run without a
prompt belongs in `permissions.allow`, not in a hook that happens to sit in the
call path.

## What gets recorded

| Hook | Event |
|---|---|
| `UserPromptSubmit` | `{author: "user", username, content}` |
| tool call (by the tool itself) | `{author: "assistant", content: "", calls: [...], result: {...}}` |
| `Stop` | `{author: "assistant", content}` |
| `StopFailure` | `{status: "failed", error}` |

A session's first prompt creates the conversation, taking its `base` from the
current `HEAD` and its fallback title from that prompt. Only a prompt may create
one: a `Stop` for an unknown session fails rather than inventing a root, so
hooks installed mid-session are a loud error and not a transcript that begins in
the middle.

The conversation id is derived from the session id rather than stored in a map,
so the ref is the whole record. `claude --resume` keeps its session id, so a
resumed session extends the same conversation.

A tool call is recorded as **one commit carrying both the call and its result**.
The protocol keys tool activity on `(request, round, tool_use_id)` and falls
back to `(commit, 0)` when those are absent (`durable_tool_scope`), so sharing a
commit is what pairs a call with its result. It also keeps `request` off the
event, which matters: `waterfall_string` would otherwise hoist it into the
conversation-level projection for a request no worker will ever run.

Nothing here writes lifecycle state. The protocol's `queued`/`running` admission
exists so a worker can claim a request, and nothing recorded this way is ever
claimed — Claude Code already ran the turn. `fold_events` defaults an
unspecified status to `idle`, so omitting admission is both honest and what
keeps `caos talk` and the TUI's `reconcile_active_requests` from trying to
resume a request that was never dispatched.

## Concurrent tool calls

Claude Code issues independent tool calls in parallel, so two mutations
routinely start from the same conversation head. Each tool runs its whole
read-modify-write *inside* the compare-and-swap retry loop: a call that loses
the CAS does not merge a stale result, it re-reads the new tree and applies
itself again.

For `edit` that is stronger than a three-way merge, because `old_string` is
re-matched against the content that actually won. For `write` it is
last-writer-wins on that one path, while every other path in the winner's tree
is preserved. Eight concurrent edits to the same file all land.

## Not yet done

- **Subagents.** `SubagentStart`/`SubagentStop` are not wired, so an `Agent`
  call records nothing. They carry `agent_id`/`agent_type` and map onto the
  existing `spawned_by` child-conversation shape.
- **Model attribution.** The `Stop` payload carries no model name, so assistant
  events have no `model` field and the TUI shows a blank where it would name
  one.
- **`bash` and project tools.** Only the four inline tools exist here. The
  dispatched tools — `bash`, `grep`, and everything under `caos-tools/` — still
  run only inside `llm-step`.
