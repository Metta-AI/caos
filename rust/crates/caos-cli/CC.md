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
do: the step that runs the tools, and NOTHING KNOWS WHERE IT LIVES. There is no
default and no convention — the caller says, in any of the image arg types, and
which one is right depends on where the caller is standing:

```text
--llm-step:@=std/llm-step                              a path in this tree
--llm-step:@@=github:<owner>/<repo>?rev=<40 hex>&dir=std/llm-step
                                                       another repo, pinned
```

`dev/claude-code/`'s configuration uses the first, because it is caos' own
checkout — that file is the caller, playing the part a person plays when they
type `caos tui --llm-step:@=…`. A session in somebody else's repository uses
the second, and that is the whole answer to "how does a session in an arbitrary
repo find the step": the client fetches the pinned tree and evaluates it
exactly as it would a local directory (design/flake-inputs.md). Nothing is
added to that repository, and only the resolved oid enters the cache key.

## What the model sees

Claude Code's built-in file and shell tools are denied, which removes them from
the model's context entirely rather than merely refusing their calls. In their
place the tool server offers **`llm-step`'s tools** — read, ls, grep, bash,
write, edit, the history tools, the caos build/test tools, `merge`, and
whatever the workspace defines under `caos-tools/`.

The deny list is `Bash, Read, Edit, Write, Glob, Grep, NotebookEdit` — every
built-in whose job a workspace tool does, so the model works in the RECORDED
conversation tree, not the container's raw checkout (which a caos `write` never
touches, so the two diverge). `Monitor` is on the list for a different reason:
it is not a file tool, but it runs an arbitrary shell `command`, so leaving it
would be a hole straight back to the shell the `Bash` deny closes — a session
was seen reading files and running commands through it while everything else was
blocked. Web access (`WebFetch`/`WebSearch`) and subagents (`Task`) are left
alone: the step offers no equivalent, so denying them would remove a capability
rather than redirect it. **Any new harness tool that can run a command or read a
path belongs on this list**; the deny list is only as tight as its last audit.

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

The cost is that `tools/list` is a worker run rather than a constant — so it
does not happen inside `tools/list`. The listing answers immediately with what
is known, a background thread resolves the step (retrying, because a cloud
session establishes its `caos` remote and its tunnel *after* spawning this
server), and `notifications/tools/list_changed` announces the real list when it
lands. Nothing in front of the handshake.

Until it lands there is exactly one tool, **`caos_status`**, and it exists
because of how the alternative failed: a model handed zero tools does not
report "my tool server has no tools", it reports that caos is absent — which is
what every session said while its server sat connected and working. `caos_status`
answers with the actual reason, including which resolution attempt failed and
why.

The tree the session's own `caos-tools/` come from is named ONLY if that
directory exists, because naming it pushes the whole working tree to the caos
server. For a repository that defines no tools that is a push of everything to
be told "none" — and through a tunnel whose far end is gone it does not fail,
it waits.

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
Both pass `--llm-step:@=std/llm-step`, which is a statement about THIS
checkout and nothing more: these two files are the caller. Elsewhere they say
something else — `dev/claude-code/cloud/configure.sh` rewrites that argument
into a locator pinned to the commit the installed client was built from, so a
session in an unrelated repository runs the step from the same tree as its
client. It runs at setup AND at every session start, because the client is
refreshed per session and the two must not drift apart.

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

## Driving a cloud session from elsewhere

A local Claude session (or a person at a terminal) can start a cloud session,
prompt it, and read everything it did — with no shell INTO the container. The
container refuses an inbound tunnel (`dumbpipe connect <endpoint> | bash` reads
as a reverse shell and trips a model refusal), and it does not need one: the
cloud session runs commands as its own tool calls, and its whole transcript —
every tool call and its stdout — is readable over the claude.ai API.

Three legs, each measured:

```bash
# start — a NEW session. `--cloud` needs a TTY, so wrap it in script(1);
# piped/non-interactive stdout makes claude ignore --cloud and run locally.
script -qec "claude --cloud 'your first prompt'" /dev/null   # prints session_…

# continue — inject another prompt into an EXISTING session. No TTY. Re-wakes
# an idle or reclaimed container from its snapshot (setup is cached, so it is
# fast). This is NOT the interactive attach path (`claude --cloud <id> 'prompt'`
# under a tty), which errors "attaching … is not enabled for your account";
# `-p` is a different, message-post route that is enabled.
claude --cloud session_XXXX -p 'your next prompt'

# read — the full transcript, from anywhere. In Claude Code this is the
# RemoteTrigger tool (action get_run_log, session_id …); by hand it is
# GET /v1/code/sessions/<id>/events. A session takes ~2 min from create to its
# first `init`, so poll after a wait.
```

Continuity does not depend on in-session memory: a cloud session against a caos
workspace keeps its durable state in the caos SERVER (the conversation ref, the
object store), reached over the iroh tunnel, so a fresh session started for the
next task picks up exactly where the last left off. The session is disposable;
the server is the thread. That is why session-per-task works as well as one
long-lived session would — and it sidesteps the account-gated interactive
attach entirely.

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
- **A cold server build.** Resolving the step is now ONE `GET /eval-locator`
  request to the caos server, which walks `.caos-expr` there — where each hop is
  sub-millisecond — instead of the ~54 chatty round trips the client used to
  make over the tunnel (measured: the difference between ~30s and ~2s, and the
  reason a cloud session's first turn works at all rather than the prompt hook
  being killed mid-resolve). The result is memoized server-side, so only the
  FIRST session against a step-tree the server has never built waits for the
  real rustc/cargo compiles — `caos_status` explains that wait — and every
  session after is a memo hit. A fully cold server therefore still risks the
  first prompt: that one request blocks on the build, which can outlast the
  hook's budget. Pre-warming the step (the `serve` resolver already resolves it
  in the background) closes that; the resolver's warm-up is not yet fenced
  against the first prompt.
