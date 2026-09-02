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
| `grep` | `pattern`, optional `path` |
| `write` | `file_path`, `content` |
| `edit` | `file_path`, `old_string`, `new_string`, optional `replace_all` |

`read`, `ls`, `write` and `edit` are the host-side counterparts of the worker's
inline tools (`std/llm-step/src/tools.rs`) and behave the same way, including the
read truncation cap and `edit`'s must-match-exactly-once rule.

`grep` is different in kind: it is the first DISPATCHED tool, running the
`std/rgrep-tool` std tool as an ordinary caos job rather than computing an
answer locally. That tool drives the `std/rgrep` fold and renders the result
itself, so nothing here is grep-specific — it goes through `run_std_tool`, the
same path `bash` and every `caos-tools/<name>` entry will use. Every level of
the fold caches on exactly (subtree hash, pattern). Two consequences worth
knowing at the prompt:

- Repeating a grep is nearly free (0.14s here against 9s cold), and so is
  re-running one after editing an unrelated part of the tree.
- A NEW pattern over the whole repo re-folds from scratch — about 8 seconds.
  Scoping with `path` is what makes it cheap, which is why the tool description
  says so.

Its output is `path:linenum:line`. Past a budget the rendering stops reading
contents but keeps counting, closing with `[truncated — N more matching
file(s)]`, so a too-broad pattern says so instead of silently returning a
prefix. That rendering lives in the tool, once: `llm-step`, this server, and
`run-tool rgrep-tool` all show the same thing because they all read the same
`report`.

**The workspace is the conversation's tree, not your checkout.** A `write` never
touches a file on disk; it produces a new tree and appends an event carrying it.
Your working copy is untouched for the whole session, exactly as it is when the
TUI runs a turn. To bring the result into your checkout, use the conversation
like any other — `caos tui`, then `Ctrl+L`.

## Configuration

Everything is checked in under `dev/claude-code/`. Build first (`nix build`),
then:

```bash
./dev/claude-code/run                 # a session against a caos workspace
./dev/claude-code/run -p 'your task'  # or headless
```

Any argument is passed through to `claude`. To drive it by hand instead, **from
the repository root**:

```bash
claude --settings dev/claude-code/settings.json \
       --mcp-config dev/claude-code/mcp.json --strict-mcp-config
```

`mcp.json` names the server `${CAOS_BIN:-./result/bin/caos}`, so it finds the
build without anything on `PATH`, and `CAOS_BIN` overrides it with another
binary (`rust/target/debug/caos-cli`, say). The default is relative, so a
by-hand launch from elsewhere will not find it; the launcher resolves an
absolute path from its own location and has no such constraint.

`settings.json` denies Claude Code's built-in file and shell tools, which
removes them from the model's context rather than merely refusing their calls,
and points every hook at `caos cc hook`. `mcp.json` declares the tool server.

**`${CLAUDE_PROJECT_DIR}` expands in a hook command but NOT in `mcp.json`.**
Claude Code sets that variable in the environment *of* a spawned stdio server;
it is not in Claude Code's own environment, which is what `mcp.json` expansion
reads. So the hooks in `settings.json` use it and `mcp.json` reads `${CAOS_BIN}`,
which `dev/claude-code/run` exports.

That distinction is worth the paragraph because of how it fails. An unset
variable in `mcp.json` is not an error: Claude Code warns, uses the literal
`${CLAUDE_PROJECT_DIR}` as the command, and still reports the server as loaded.
The session then runs with **no caos tools at all** — and a model with no tools
does not say so. Asked to write a file, it will emit a plausible `bash` block
and report success, having written nothing. `dev/claude-code/run` therefore
probes `tools/list` before launching, so a missing or stale binary is an error
at startup instead of a fabricated result later.

## When the server shows as broken

`/mcp` reports a failed server but no reason, and neither the terminal nor
`--debug` prints one. The error is in a per-server log:

```bash
ls -t ~/.cache/claude-cli-nodejs/*"$(pwd | tr / -)"*/mcp-logs-caos/ | head -1
```

or just look under
`~/.cache/claude-cli-nodejs/<project-dir-with-slashes-as-dashes>/mcp-logs-caos/`
and read the newest `.jsonl`. One line per connection attempt, and the failure
is explicit:

```json
{"error":"Connection failed (ENOENT): Executable not found in $PATH: \"caos\""}
```

That is the log to check first for any "broken server" report — a wrong path,
a binary too old to know `cc serve`, or a crash on startup all land there and
nowhere else.

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
- **`bash` and project tools.** `grep` proved the dispatch path; `bash` and
  everything under `caos-tools/` still run only inside `llm-step`. Until they
  land the model cannot build, test, or run anything.
