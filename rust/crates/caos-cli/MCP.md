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
caos mcp serve    a stdio bridge to the server's tool server (POST /mcp)
```

Both are told, by argument, everything they need. **Neither reads a checkout.** A
process has no repository to be standing in, and a path (`--llm-step:@=std/llm-step`)
is refused by name because it would name a file in one:

```text
--server=<url>                the caos server
--llm-step:@@=<git locator>   the step that runs the tools, pinned: git+…?rev=<40 hex>&dir=std/llm-step
--base:@@=<git locator>       what a NEW conversation starts from (the hook; optional)
--secret-readers=<key …>      the secret reader keys this process presents (else the checkout's git config)
```

A locator is resolved by the server (`:@@=`, design/flake-inputs.md), so a harness
that wants uncommitted files in a conversation commits and pushes them first and
names the commit. `--llm-step:hash=<oid>` also works, for a step the caller already
holds. The content locator names the commit's own tree, not what evaluating it
produces (`raw=1`, added when absent), because content is code to read. A first
prompt can name another with a leading `/caos-session-start-from <locator>` line.

## Where the tool server lives

**In the caos server, at `POST /mcp`** (`rust/crates/server/src/mcp.rs`). The
handshake, the `instructions`, the constant `tools/list` and the decoding of a
call's result are all there, so a harness adds no copy of them. One JSON-RPC
message per request, answered in the body (`202` with no body for a
notification), over MCP's streamable HTTP transport minus the optional event
stream: nothing is pushed to a client.

A request says who it is in headers, because the endpoint keeps nothing between
messages:

| header | |
|---|---|
| `X-Caos-Step` | the step that runs the tools: a git locator, or a bare 40-hex hash |
| `X-Caos-Secret-Readers` | the reader keys, as on every other endpoint |

The conversation is not a header. Each tool call names its session in the
`caos_session` argument, which the harness's PreToolUse hook fills in, and the
conversation id derives from it (`cc/<session>`).

A harness that can reach the server by URL points its MCP config at `/mcp` and
needs no caos binary. **One that can only reach it through `caos://`** (a cloud
container dials out and listens for nothing, and `caos://` is a transport only
our client speaks) runs `caos mcp serve`, which is now a bridge: a line on stdin
goes to `/mcp`, the answer comes back as a line on stdout. The handshake and the
tool list it answers itself, from the constants in `caos-world` that the server
uses too, because a harness drops a tool server whose `initialize` is slow, and one
that crossed an iroh relay from a cold container was. It adds two things the
server cannot know: this container's diagnostics on `help()`, and a
reason, in place of an error, when the server cannot be
reached.

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
| `start(path, arguments)` | begin the same call and return a ticket. The same call always has the same ticket, and starting one that has finished does not run it again. It is `run` without the wait: `run` starts the call and waits for its ticket, and the ticket is the one `caos-cli run` prints, so `caos-cli watch` takes it. |
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
the session then ran somewhere caos had not been told about.

Each call is ONE request to the server: `run` is `POST /eval/run` of `llm-step` in its
tools-only mode, `start` is `POST /start/run`, `result` is `GET /result/<ticket>`
(see [HTTP API](../../../spec/http-api.md)). The step declares the call (an assistant message carrying its
`tool_use`), runs the tool, and answers with the call's own record, so this client neither
writes the conversation nor reads it back. The tools run exactly as they run in a turn
the step drives itself, and a tool reads and behaves one way whether a model meets it
here or in the tui.

**A path that does not exist is the model's mistake, and it is told so.** `help` or `run`
at a path nothing evaluates comes back as an error result, naming the sibling
directories, not as a failed server.

When there is no server (none named by argument or environment), `help` with no path
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
Both carry the placeholder `CAOS_MCP_ARGS_HERE` where those arguments go: `run`
fills it from `CAOS_SERVER_URL`, `CAOS_STEP` (by default the server's own
`refs/caos/dev`) and `CAOS_BASE`, and the cloud installer fills it with the
server, a step locator pinned to the commit the installed client was built from,
and the content locator its setup computed.

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
`forked`, `recorded` or `nothing`) and `head`.

The one thing the step cannot find is a NEW conversation's content: the locator
`--base:@@=` names (or a `/caos-session-start-from` line does) goes to the step
as `content-locator`, and the step fetches the COMMIT it names, with its history
(from the server when it has it, else from the repository, with the GitHub token
`import-source` would use). A root parents that commit and starts from its tree;
a fork merges it in (above); the push carries the history to the server. A
conversation that exists needs none, so the locator matters only for a session's
first prompt.

A checkout therefore holds only what it fetches. Anything that reads a
conversation's ref from the local repository must fetch it first (`git fetch
caos +<ref>:<ref>`); it used to be current because the hook pushed from here.

| Hook | Transitions |
|---|---|
| `UserPromptSubmit` | `conversation.root` (the first only), `message.append` (the prompt) |
| tool call | the step's own `message.append` of an assistant message declaring the call, then its `tool.start` and `tool.complete` -- this process writes nothing |
| `Stop` | `message.append` (the closing message, as the assistant's) -- nothing when there is none |
| `StopFailure` | `message.append` (the error, as a system message) |

**The transcript is flat: facts, appended as they happen.** A prompt, an
assistant message, a call, a call's result. Nothing in a conversation says which
turn is running, so there is nothing to open on a prompt, hold for a running
call, or close on a `Stop`. That matters because Claude Code drives calls
asynchronously: it moves any MCP call still running after 120s to the
background, ends the turn, opens new ones, and delivers the result later. The
call's result is a fact recorded whenever it arrives, after whatever came in
between, and its `tool_use` is answered by the record keyed by its id. A call
that never finishes is a `tool_use` with no result, which a reader shows as
unfinished.

A session's first prompt creates the conversation, taking its `base` from the
current `HEAD` and its fallback title from that prompt. Only a prompt may create
one: a `Stop` for an unknown session fails rather than inventing a root, so
hooks installed mid-session are a loud error and not a transcript that begins in
the middle.

The conversation id is derived from the session id rather than stored in a map,
so the ref is the whole record. `claude --resume` keeps its session id, so a
resumed session extends the same conversation.

**A call is one request to the server.** `POST /eval/run` of the step, as a
form: `base:hash` (the step), `conversation`, `tools-only=<tool_use_id>`, `tool`
and `arguments` (JSON). The step declares the call -- an assistant message carrying
its `tool_use`, unless one already does -- and
runs it, recording starting it and completing it. The answer is the call's own
record, a tree of `status`, `observation` and `conversation`, so this process
writes nothing and reads nothing back from the conversation. The protocol's first
invariant — record an action before launching it — is the step's to keep here
as everywhere, so a long tool is visible in the tui while it runs and a session
that dies mid-call leaves a record that it was attempted. Each call is its own
ArgTree because its id is in it, and a retry of the same call finds it already
declared.

`start` answers with a ticket at once, and the job declares and runs the call
whenever it gets there. The model may end its turn first; that changes nothing
for the call.

Claude Code does not expose the model's round number and does not need to: a
call is keyed by its `tool_use_id`, which is unique for the whole session, and
the step reads back each call by that id, wherever in the transcript it was
declared.

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

**The fork brings the session's client repo in.** The hook sends its content
locator (`--base:@@=`, or the prompt's `/caos-session-start-from` line) with the
fork, and the step merges that commit into the forked conversation the way git
merges two branches: a conversation's root PARENTS the commit its content came
from, so the conversation and the client repo share history and `git merge-tree`
finds the base itself. The forked conversation's files that the repo changed
since -- `flake.lock`, `.caos-expr`, the instructions -- take the repo's version;
its own work is untouched; `.caos/` is never merged. The merged commit is the
fork's second parent, so the next fork merges from there. A conflict refuses the
fork, naming the files, because nobody is in the conversation yet to resolve it;
fork from a session whose content locator is the conversation's own, or fix the
repository first. This is how a conversation recorded against one caos is picked
up after the client repo moved its pin: a fix deployed to caos reaches the
conversation on its next fork, tools included.

`cloud/install.go` writes the command into Claude Code's user config. The hook
acts on a session's **first prompt** (`fork.rs`): it recognises the command and hands the commit to the step (`llm-step --record=prompt --from=<hash>`), which sets
up the conversation, and only then records the prompt as usual — so history and
workspace are in place before the model's first turn. The command's own text is
what tells the MODEL to go and read that history; the hook cannot put it in the
model's context. Its first line is a `caos-conversation-command:` marker, which
is what the hook finds if Claude Code passes it the command's expansion rather
than the `/name <hash>` that was typed.

**An incomplete head is forked as it is.** A session that died mid-turn leaves a
running request, and perhaps calls that started and never finished. A conversation
is a flat transcript, so the fork copies it unchanged: an unfinished call is a
`tool_use` with no `tool_result`, which a reader renders as unfinished and a
resuming driver decides what to do with. The protocol used to refuse such a fork,
so the source had to be "settled" first — each open call completed as `cancelled`,
the request ended `interrupted` — before it could be forked; that step is gone.

## Not yet done

- **Subagents.** `SubagentStart`/`SubagentStop` are not wired, so Claude Code's
  own `Agent` call records nothing. (`agent-spawn` and `run-async` are a
  different thing and ARE reachable, as `caos-std/agent-spawn` and the like, but
  nothing here has exercised them from Claude Code yet.)
- **Model attribution.** The `Stop` payload carries no model name, so assistant
  entries say `claude-code` rather than naming a model. Better than a
  plausible-looking string nothing verified.
