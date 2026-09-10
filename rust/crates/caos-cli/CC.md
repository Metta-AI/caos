# `caos cc` — Claude Code against a caos workspace

`caos cc` lets Claude Code drive a caos conversation. Claude Code runs the
model; caos keeps the durable log and owns the workspace. The result is an
ordinary conversation — the same head ref (its id is `cc/<session-id>`, which
v3 hex-encodes into the ref path), the same append-only spine, and the same
replay `caos tui` performs for a conversation it started itself
(design/chat.md).

Two commands make that work:

```text
caos cc hook     record one Claude Code hook payload (JSON on stdin)
caos cc serve    the workspace tool server (JSON-RPC on stdio)
```

Both take `--llm-step:<type>=<value>`, exactly as `caos tui` and `caos chat`
do — the step that runs the tools, named as a path into whatever tree you are
in (`std/llm-step` here, `caos-std/llm-step` in a repo that mounted caos).

## What the model sees

Claude Code's built-in file and shell tools are denied, which removes them from
the model's context entirely rather than merely refusing their calls. In their
place the tool server offers **`llm-step`'s tools** — read, ls, grep, bash,
write, edit, the history tools, the caos build/test tools, `merge`, and
whatever the workspace defines under `caos-tools/`.

Not a copy of them: THE SAME ONES. `tools/list` runs the step with
`--list-tools` and hands back the registry it answers with, and a call runs the
step with `--tools-only`, which executes it exactly as it does for a turn the
step drives itself. So this client knows what `edit` is only in the sense that
it passes the name along, and a tool reads and behaves one way whether a model
meets it here or in the tui.

That also settles what used to be a standing hazard: the tools were once
implemented twice, and the two copies drifted — a second description of every
tool, a second bash-input shape (and so a second cache key for an identical
command), a second rendering of a grep result. What is offered here now is
whatever the named step offers, in whatever repository it is pointed at.

The cost is that `tools/list` is a worker run rather than a constant, so a
session start needs the caos server up. It is memoized on the step and the
workspace tree, so it is a cache hit for every session after the first.

**The workspace is the conversation's tree, not your checkout.** A `write`
never touches a file on disk; it produces a new tree and the step appends a
transition carrying it. Your working copy is untouched for the whole session,
exactly as it is when the TUI runs a turn. To bring the result into your
checkout, use the conversation like any other — `caos tui`, then `Ctrl+L`.

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
Both pass `--llm-step:@=std/llm-step`, caos' own path to the step; a repository
that mounts caos elsewhere edits that path in those two files, as it would for
`caos tui --llm-step:@=…`.

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
probes `tools/list` before launching, so a missing binary — or a caos server
that is not up, since the listing comes from the step — is an error at startup
instead of a fabricated result later.

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

| Hook | Transitions |
|---|---|
| `UserPromptSubmit` | `message.append` (the prompt), `request.admit`, `request.claim` |
| tool call | `model.complete` declaring the call — then the step's own `tool.start` and `tool.complete` |
| `Stop` | `model.complete` (the closing message), `request.terminal` idle |
| `StopFailure` | `message.append` (the error), `request.terminal` failed |

A session's first prompt creates the conversation, taking its `base` from the
current `HEAD` and its fallback title from that prompt. Only a prompt may create
one: a `Stop` for an unknown session fails rather than inventing a root, so
hooks installed mid-session are a loud error and not a transcript that begins in
the middle.

The conversation id is derived from the session id rather than stored in a map,
so the ref is the whole record. `claude --resume` keeps its session id, so a
resumed session extends the same conversation.

**The prompt admits a REAL request** — an ArgTree over the step, prepared the
way the tui prepares a turn, and claimed in the same compare-and-swap. Claimed
here rather than by a runner because the turn is already running by the time the
hook fires; and real rather than a synthetic id because every tool call of that
turn RUNS it (`--run=<request> --tools-only=<call id>`, one fresh ArgTree per
call, since the request is itself an ArgTree and a reused one would be answered
from the first call's memo).

So the two halves of a call have different authors, and that is the design: cc
declares the call as the model's own (v3 accepts no tool that no message
declared) and the step records starting it, running it and completing it. The
protocol's first invariant — record an action before launching it — is the
step's to keep here as everywhere, so a long tool is visible in the tui while it
runs and a session that dies mid-call leaves a record that it was attempted.

Claude Code does not expose the model's round number and does not need to: the
declaration opens a round, the call belongs to the round that declared it, and a
`tool_use_id` is unique for the whole session.

**Tool execution is serial**, matching `llm-step`'s single queue. The tool server
reads and handles one JSON-RPC request at a time, so a batch of parallel calls
from the model executes one after another, each starting from the head the
previous one left. The compare-and-swap retries are therefore not a concurrency
model — they protect against another writer, such as an interjection typed into
the tui against the same conversation.

## Not yet done

- **Subagents.** `SubagentStart`/`SubagentStop` are not wired, so Claude Code's
  own `Agent` call records nothing. (`spawn_agent` and `run_async` are a
  different thing and ARE offered — they are the step's tools, so they arrive
  with everything else — but nothing here has exercised them yet.)
- **Model attribution.** The `Stop` payload carries no model name, so assistant
  entries say `claude-code` rather than naming a model. Better than a
  plausible-looking string nothing verified.
- **A cheaper listing.** `tools/list` is a worker run, so a cold session start
  waits for one. It is memoized, but the first one in a new tree is not free.
