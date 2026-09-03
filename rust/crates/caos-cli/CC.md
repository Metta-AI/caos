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
| `bash` | `cmd`, optional `paths` |
| `caos-build` | — |
| `caos-test` | optional `only`, `test-salt` |
| `caos-test-result` | `hash`, optional `log` |
| `write` | `file_path`, `content` |
| `edit` | `file_path`, `old_string`, `new_string`, optional `replace_all` |

`read`, `ls`, `write` and `edit` are the host-side counterparts of the worker's
inline tools (`std/llm-step/src/tools.rs`) and behave the same way, including the
read truncation cap and `edit`'s must-match-exactly-once rule.

`caos-build`, `caos-test` and `caos-test-result` are the harness's own std
tools, offered here exactly as `llm-step` offers them. Their parameters are not
written down in this client at all: they are read from the `help` each image
carries and parsed by the same rules `parse_help` uses, so adding a std tool is
a one-line change and rewording one needs no change here.

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

`bash` runs a command through `std/bash-tool` — the same sandbox the tui uses,
where only the workspace paths listed in `paths` are materialized and touching
any other existing path fails with EACCES and a retry hint naming it. It is the
one tool here whose result advances the workspace: the command's output tree
becomes the conversation's new one. A non-zero exit is a value, not a failure —
the model reads stderr and reacts — and the workspace still advances, since the
command may have written files before it failed.

The `{tree, cmd, paths}` input is built byte for byte the way `llm-step` builds
it, which matters because the ArgTree is the cache key: the same command run
from the tui and from Claude Code is ONE cached job. Passing `bash-tool`'s
direct `--cmd`/`--tree` arguments instead would work and would silently fork
the cache.

`grep`'s output is `path:linenum:line`. Past a budget the rendering stops reading
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
./dev/claude-code/remote-control      # driven from claude.ai/code or the app
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

## Remote Control

```bash
./dev/claude-code/remote-control
```

Then connect from claude.ai/code or the mobile app. Arguments pass through to
`claude remote-control` (`--name`, `--spawn`, …).

It needs its own launcher because `claude remote-control` accepts **no**
`--settings` or `--mcp-config`: it is a persistent server, so it reads
configuration from the usual places. The usual place for this repo would be
`.claude/`, which applies to every Claude Code session here — and since Claude
Code reloads settings files live, dropping the deny list there would disarm an
ordinary session already running in this checkout. So the launcher builds a
throwaway config dir under `.git/caos-remote-control` and points
`CLAUDE_CONFIG_DIR` at it, scoping everything to the one invocation.

Two things about that dir are worth knowing:

- **It needs claude.ai subscription auth.** Remote Control refuses outright when
  `ANTHROPIC_API_KEY` is set — even to an empty string, since it tests whether
  the variable is set at all — so the launcher unsets it. `.credentials.json` is
  symlinked to the real one, and `~/.claude.json` is copied, because Remote
  Control also reads account and org fields from it and refuses without them.
  The copy is 0600, like the original.
- **The dir persists, because that is where sessions live.** Transcripts land in
  its `projects/`, and the session records Remote Control writes land in its
  `.claude.json` — so the launcher seeds that file once and never re-copies it.
  Rebuilding the dir on each launch is what used to make `--continue` and the
  session list on claude.ai come up empty after a restart. To start over, delete
  it: `rm -rf .git/caos-remote-control`.
- **The account fields are a snapshot.** `claude auth login` refreshes the
  credentials the dir symlinks, but not the org information copied into
  `.claude.json` when it was seeded. If Remote Control starts refusing on
  eligibility again, delete the dir so it reseeds. Any other user-scope MCP
  servers you have also come along for the ride; the caos tool server is
  declared into the copy with `claude mcp add --scope user`, so your real config
  is never modified.

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
| `Stop` | `{author: "assistant", content}` |
| tool call | `{request, round, author: "assistant", content: "", calls: [...]}` then `{request, round, result: {...}}` |
| `StopFailure` | `{status: "failed", error}` |

A session's first prompt creates the conversation, taking its `base` from the
current `HEAD` and its fallback title from that prompt. Only a prompt may create
one: a `Stop` for an unknown session fails rather than inventing a root, so
hooks installed mid-session are a loud error and not a transcript that begins in
the middle.

The conversation id is derived from the session id rather than stored in a map,
so the ref is the whole record. `claude --resume` keeps its session id, so a
resumed session extends the same conversation.

A tool call is recorded as **two events, the call before the tool runs and the
result after** — the protocol's first invariant ("record an action before
launching it and a result before consuming it") and exactly what `llm-step`
does. So a long tool is visible in the tui while it runs rather than appearing
only once it finishes, and a session that dies mid-call leaves a record that it
was attempted. The call event is tree-neutral; the result event carries the
workspace the tool produced.

Both carry the same `(request, round)`, which is how the fold pairs them
(`durable_tool_scope`). `request` is the turn, derived by hashing Claude Code's
`prompt_id` into a git blob so it is a canonical object id like `llm-step`'s
`run`. Claude Code does not expose the model's round number and does not need
to: a `tool_use_id` is unique for the whole session, so one round per turn pairs
exactly as `llm-step`'s calls do within a round, which is the only place the
number does any work. Nothing dispatches that request, because nothing here ever
writes `queued` or `running`.

**Tool execution is serial**, matching `llm-step`'s single queue. The tool server
reads and handles one JSON-RPC request at a time, so a batch of parallel calls
from the model executes one after another, each starting from the head the
previous one left. The compare-and-swap retries are therefore not a concurrency
model — they protect against another writer, such as an interjection typed into
the tui against the same conversation.

Nothing here writes lifecycle state. The protocol's `queued`/`running` admission
exists so a worker can claim a request, and nothing recorded this way is ever
claimed — Claude Code already ran the turn. `fold_events` defaults an
unspecified status to `idle`, so omitting admission is both honest and what
keeps `caos talk` and the TUI's `reconcile_active_requests` from trying to
resume a request that was never dispatched.

## Not yet done

- **Subagents.** `SubagentStart`/`SubagentStop` are not wired, so an `Agent`
  call records nothing. They carry `agent_id`/`agent_type` and map onto the
  existing `spawned_by` child-conversation shape.
- **Model attribution.** The `Stop` payload carries no model name, so assistant
  events have no `model` field and the TUI shows a blank where it would name
  one.
- **Project tools.** `caos-tools/<name>` entries are not offered yet; they are
  discovered from the workspace rather than resolved from a fixed path, which is
  the one piece `run_tool` does not do. This repo has no `caos-tools/` of its
  own, so today they would register nothing here anyway.
- **The history tools** — `log`, `show`, `diff`. Their docs are now data
  (`std/llm-step/src/githist/*.help`, read by the same `parse_help`), so the
  descriptions are ready to share; what is not solved is handing the worker the
  workspace COMMIT. `llm-step` binds it from a `/cas` path, where the object is
  already present. From the host it has to be a gitlink, and git does not carry
  a gitlink's target in a tree's push closure — the worker then fails with
  `object not found on this server` for a commit the server demonstrably holds
  (`HEAD /object/<oid>` is 200, and a worker can `caos get-hash` it). Neither
  `ensure_pushed` (which short-circuits, since the server does hold it) nor an
  explicit `refs/caos/req/<oid>` push changed that.
- **`merge`.** Its result is a COMMIT, not a value, and it is the one tool that
  advances the conversation's ancestry — `llm-step` has a dedicated callback arm
  for it, so it is not a copy of the `grep` path.
- **`spawn_agent` / `run_async`.** The independent-work pair. `run_async` is the
  answer to a long build outliving its turn.
