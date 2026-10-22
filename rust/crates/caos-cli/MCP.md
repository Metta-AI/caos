# `caos mcp` — Claude Code against a caos workspace

`caos mcp` lets Claude Code drive a caos conversation. Claude Code runs the
model; caos keeps the durable log and owns the workspace. The result is an
ordinary conversation — the same head ref (its id is `cc/<session-id>`, which
v3 hex-encodes into the ref path), the same append-only spine, and the same
replay `caos tui` performs for a conversation it started itself
(design/chat.md).

Two commands make that work:

```text
caos mcp hook     record one Claude Code hook payload (JSON on stdin)
caos mcp serve    the workspace tool server (JSON-RPC on stdio)
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

`integrations/claude-code/`'s configuration uses the first, because it is caos' own
checkout — that file is the caller, playing the part a person plays when they
type `caos tui --llm-step:@=…`. A session in somebody else's repository uses
the second, and that is the whole answer to "how does a session in an arbitrary
repo find the step": the client fetches the pinned tree and evaluates it
exactly as it would a local directory (design/flake-inputs.md). Nothing is
added to that repository, and only the resolved oid enters the cache key.

## What the model sees

**Five fixed tools, and they never change:** `run`, `help`, `start`, `result` and
`eval-path`. They are the caos server's own primitives — run something, describe it,
begin it without waiting, ask how it went, evaluate a path — and what they run is
named by a PATH in the conversation: `caos-std/read`, `caos-std/bash-tool`,
`feature/01-change/caos-tools/test`. Nothing is listed and nothing has to resolve
first, so `tools/list` is a constant and a client that reads it once, or ignores
`tools/list_changed`, is correct rather than stale.

| tool | what it does |
|---|---|
| `help(path)` | what the tool at a path does and the arguments it takes. `help()` with no path says how caos is doing in this session: the conversation and its head commit, and diagnostics. |
| `run(path, arguments)` | run the tool at a path and wait for its output. |
| `start(path, arguments)` | begin the same call and return a ticket. The same call always has the same ticket, and starting one that has finished does not run it again. |
| `result(ticket)` | pending, or the output, or how it failed. Asking never runs anything. |
| `eval-path(path)` | evaluate a path and return the hash of what it produces (`caos-std` exists only this way). Pass the hash as `in` to `read`, `ls` or `grep` to look inside it. |

The model learns the rest from the server's `instructions`, which the handshake
carries and a client puts in front of the model, and from `help`. Both say where the
standard tools are (`caos-std/`: `read`, `ls`, `write`, `edit`, `copy`, `move`,
`remove`, `grep`, `log`, `show`, `diff`, `merge`, `import-source`, `publish-source`,
`bash-tool`, `caos-build`, `caos-test`, `github`, …) and how to find the index
(`eval-path("caos-std")`, then `read` its `README.md`). `std/README.md` is the index.

**caos is a separate git workspace, not a replacement for Claude Code's own tools.**
The conversation was seeded from a commit, not from the container's checkout, so
nothing diverges: Claude Code's `Read`, `Edit` and `Bash` act on the container's
files, and caos's tools act on the conversation's. Nothing moves between the two
unless the model copies it (`import-source` brings code in; `publish-source` sends a
source tree out). `shared/settings.json` therefore denies none of the built-ins. It
still denies the GitHub MCP server's tools that caos covers, and `add_repo`: a second
source was seen restarting the session in `/home/user`, which is no repository, and
the server then could not find its workspace.

Each call is ONE request to the server: `run` is `POST /eval/run` of `llm-step` in its
tools-only mode, `start` is `POST /start/run`, `result` is `GET /result/<ticket>`
(HTTP API, `/eval/<verb>`). The step declares the call on the conversation's active
request, runs the tool, and answers with the call's own record, so this client neither
writes the conversation nor reads it back. The tools run exactly as they run in a turn
the step drives itself, and a tool reads and behaves one way whether a model meets it
here or in the tui.

**A path that does not exist is the model's mistake, and it is told so.** `help` or `run`
at a path nothing evaluates comes back as an error result, naming the sibling
directories, not as a failed server.

When there is no workspace (the hook could not find a repository), `help` with no path
still answers and leads with the reason; every other tool says the same. That one call is
the whole of the diagnostic surface: a model handed a server that says nothing cannot
tell it from no server at all.

## Configuration

Everything is checked in under `integrations/claude-code/`. Build first (`nix build`),
then:

```bash
./integrations/claude-code/run                 # a session against a caos workspace
./integrations/claude-code/run -p 'your task'  # or headless
```

Any argument is passed through to `claude`. To drive it by hand instead, **from
the repository root**:

```bash
claude --settings integrations/claude-code/shared/settings.json \
       --mcp-config integrations/claude-code/shared/mcp.json --strict-mcp-config
```

`mcp.json` names the server `${CAOS_BIN:-./result/bin/caos}`, so it finds the
build without anything on `PATH`, and `CAOS_BIN` overrides it with another
binary (`rust/target/debug/caos-cli`, say). The default is relative, so a
by-hand launch from elsewhere will not find it; the launcher resolves an
absolute path from its own location and has no such constraint.

`settings.json` points every hook at `caos mcp hook` (and denies the GitHub tools caos covers, below).
`mcp.json` declares the tool server.
Both pass `--llm-step:@=std/llm-step`, which is a statement about THIS
checkout and nothing more: these two files are the caller. Elsewhere they say
something else — `install.sh --user-config` rewrites that argument
into a locator pinned to the commit the installed client was built from, so a
session in an unrelated repository runs the step from the same tree as its
client. It runs at setup AND at every session start, because the client is
refreshed per session and the two must not drift apart.

**`${CLAUDE_PROJECT_DIR}` expands in a hook command but NOT in `mcp.json`.**
Claude Code sets that variable in the environment *of* a spawned stdio server;
it is not in Claude Code's own environment, which is what `mcp.json` expansion
reads. So the hooks in `settings.json` use it and `mcp.json` reads `${CAOS_BIN}`,
which `integrations/claude-code/run` exports.

That distinction is worth the paragraph because of how it fails. An unset
variable in `mcp.json` is not an error: Claude Code warns, uses the literal
`${CLAUDE_PROJECT_DIR}` as the command, and still reports the server as loaded.
The session then runs with **no caos tools at all** — and a model with no tools
does not say so. Asked to use caos, it will describe doing it and report success,
having done nothing. `integrations/claude-code/run` therefore probes `tools/list`
before launching, so a missing binary is an error at startup instead of a fabricated
result later.
## Driving a cloud session from elsewhere

A local Claude session (or a person at a terminal) can start a cloud session,
prompt it, and read everything it did — with no shell INTO the container. The
container refuses an inbound tunnel (piping a network connection into a shell
reads as a reverse shell and trips a model refusal), and it does not need one: the
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
object store), reached over its `caos://` ticket, so a fresh session started for
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
a binary too old to know `mcp serve`, or a crash on startup all land there and
nowhere else.

## What gets recorded

**The step writes all of it.** A hook sends one request to the server --
`llm-step --record=prompt|stop|failure`, with the event's text, an actor and a
message id the hook mints -- and the step makes these transitions on the
conversation. This process writes no part of a conversation, for a hook or a
tool call. A repeated event names the message id it already recorded and writes
nothing, so a retried hook is harmless. The answer is `status` (`created`,
`recorded`, `joined`, `held` or `nothing`) and `head`.

The one thing the step cannot find is a NEW conversation's content: the base
this checkout stands in (or a `/caos-session-start-from` names) is resolved
and pushed here, and its tree is handed to the step. A conversation that exists
needs none, so the base is resolved only for a session's first prompt.

A checkout therefore holds only what it fetches. Anything that reads a
conversation's ref from the local repository must fetch it first (`git fetch
caos +<ref>:<ref>`); it used to be current because the hook pushed from here.

| Hook | Transitions |
|---|---|
| `UserPromptSubmit` | `conversation.root` (the first only), `message.append` (the prompt), `request.admit`, `request.claim` — or, while a request is held open, `request.interject` or a `request.terminal` first (below) |
| tool call | the step's own `model.complete` declaring the call, then its `tool.start` and `tool.complete` — this process writes nothing |
| `Stop` | `model.complete` (the closing message), `request.terminal` idle — nothing, while a call is still running |
| `StopFailure` | `message.append` (the error), `request.terminal` failed — the terminal only once no call is running |

**A call can outlive its turn.** Claude Code moves any MCP call still running
after 120s to the background, ends the turn, and delivers the result later in a
turn of its own, which it opens with a `UserPromptSubmit`. Only a running
request accepts a call's `tool.complete`, so a `Stop` that finds a call
outstanding leaves the request open, and records no closing message, since a
`model.complete` would open a new round the finishing call is then checked
against. The next prompt closes that request if its calls have finished, and
otherwise joins it as an interjection. A request closed under a running call
has nowhere to record its result: the step finds it terminal and records
nothing, and the model receives an empty error ("Task failed: no detail") for
a call that succeeded.

A session's first prompt creates the conversation, taking its `base` from the
current `HEAD` and its fallback title from that prompt. Only a prompt may create
one: a `Stop` for an unknown session fails rather than inventing a root, so
hooks installed mid-session are a loud error and not a transcript that begins in
the middle.

The conversation id is derived from the session id rather than stored in a map,
so the ref is the whole record. `claude --resume` keeps its session id, so a
resumed session extends the same conversation.

**The prompt admits a request and claims it in the same compare-and-swap.**
Claimed here rather than by a runner because the turn is already running by the
time the hook fires. The request is identified by the commit of the prompt that
opened it -- unique, and known before the admission that names it is written --
and it is what the step finds as the conversation's active one when a call
arrives. It is not an ArgTree anyone can run: no runner will pick it up, and a
call names no request.

**A call is one request to the server.** `POST /eval/run` of the step, as a
form: `base:hash` (the step), `conversation`, `tools-only=<tool_use_id>`, `tool`
and `arguments` (JSON). The step declares the call on the conversation's active
request as the model's own — v3 accepts no tool that no message declared — and
runs it, recording starting it and completing it. The answer is the call's own
record, a tree of `status`, `observation` and `conversation`, so this process
writes nothing and reads nothing back from the conversation. The protocol's first
invariant — record an action before launching it — is the step's to keep here
as everywhere, so a long tool is visible in the tui while it runs and a session
that dies mid-call leaves a record that it was attempted. Each call is its own
ArgTree because its id is in it, and a retry of the same call finds it already
declared.

The declaration lands a moment after the request reaches the server, not before
it as it did when this process wrote it, so a `Stop` in that gap sees no call.
Claude Code backgrounds a call only after 120s, long after the step has started.

Claude Code does not expose the model's round number and does not need to: the
declaration opens a round, the call belongs to the round that declared it, and a
`tool_use_id` is unique for the whole session.

**Tool execution is serial**, matching `llm-step`'s single queue. The tool server
reads and handles one JSON-RPC request at a time, so a batch of parallel calls
from the model executes one after another, each starting from the head the
previous one left. The compare-and-swap retries are therefore not a concurrency
model — they protect against another writer, such as an interjection typed into
the tui against the same conversation.

## Picking a conversation up in a new session

A cloud session is disposable and its Claude Code context dies with it; the
conversation does not. One slash command takes the conversation COMMIT to start
from — the `caos head <hash>` a turn prints, or `conversation head:` from `help`
with no path. A commit, because it is immutable where a ref moves.

`/fork-caos-conversation <hash>` makes a NEW branch: the session's own
`cc/<session>` conversation, made by `conversation.fork` on `<hash>`. The original
does not move. There is no command that continues the SAME branch from a new
session: a session's conversation is always the one its id derives, so nothing
about it is kept locally.

`cloud/install.go` writes the command into Claude Code's user config. The hook
acts on a session's **first prompt** (`resume.rs`): it recognises the command, sets
up the conversation, and only then records the prompt as usual — so history and
workspace are in place before the model's first turn. The command's own text is
what tells the MODEL to go and read that history; the hook cannot put it in the
model's context. Its first line is a `caos-conversation-command:` marker, which
is what the hook finds if Claude Code passes it the command's expansion rather
than the `/name <hash>` that was typed.

**An incomplete head is used as it is.** A session that died mid-turn leaves a
running request, and perhaps calls that started and never finished. The protocol
refuses to fork such a conversation, so `settle` first appends commits that close
it the way `llm-step` does for a cancelled turn: each open call completes as
`cancelled` (its observation says the session ended before it finished), and the
request ends `interrupted`. Nothing is rewritten, and these commits land on the
NEW branch, so the original is not touched. A pending publication is not settled
— it is a fact about a remote — and the protocol refuses that fork with its own
message.

## Not yet done

- **Subagents.** `SubagentStart`/`SubagentStop` are not wired, so Claude Code's
  own `Agent` call records nothing. (`agent-spawn` and `run-async` are a
  different thing and ARE reachable, as `caos-std/agent-spawn` and the like, but
  nothing here has exercised them from Claude Code yet.)
- **Model attribution.** The `Stop` payload carries no model name, so assistant
  entries say `claude-code` rather than naming a model. Better than a
  plausible-looking string nothing verified.
