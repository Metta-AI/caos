# HTTP API

The caos server listens on one port and speaks plain HTTP. This page is the
reference for the endpoints a client uses to *drive* caos: start a request and
get a ticket, wait for or poll the ticket, evaluate an expression in one call,
and the MCP tool server built on top of them. (The storage, Git and runner
endpoints are listed at the end; they have their own documents.)

Source: `rust/crates/server/src/main.rs` (`route`), `compute.rs` ("Expression
lines", `start_work`) and `mcp.rs`. Tests: `tests/expression-http`,
`tests/llm-tools-only-http`.

## The model: a run is a start whose caller waits

Every request a client makes is a **start** that returns a **ticket**; what
differs is only whether the caller then waits.

```text
POST /start/<verb>?<params>   -> <ticket>        an expression (a .caos-expr line)
POST /start  (JSON body)      -> <ticket>        a bundled request (what `caos-cli run` sends)
GET  /result/<ticket>         -> pending | ok <kind> <hash> | failed <status> <message>
GET  /wait/<ticket>           -> <kind> <hash>   blocks until the ticket has an answer
GET|POST /eval/<verb>?<params> = POST /start/<verb> followed by GET /wait/<ticket>
```

The server does all the work on a thread of its own, **under the ticket**:
forming the request (building a base, fetching a locator) and running it. The
ticket is a node of its own in the trace, and whatever it evaluates or dispatches
is recorded as its children, so `GET /status/<ticket>` shows the evaluation as
well as the run. `/eval/<verb>` is not a separate code path: it makes the same
ticket, runs the same evaluation on the same thread, and leaves the same trace.

### Tickets

A ticket is a 40-hex name derived from the request's content, so the same call
always has the same ticket. What goes into it depends on how the request was
stated:

| started with | the ticket covers |
|---|---|
| `/start/<verb>` or `/eval/<verb>` | the verb, the salt, the (sorted) arguments, the secret scope |
| `POST /start` | the bundle hash, the base type and value, the locators, the salt, the secret scope |

The secret scope is part of the ticket because the readers a caller presents
change what an evaluation produces. Consequences:

- Starting a call that is **already running** joins it.
- Starting one that has **succeeded** returns its ticket without running it again.
- Starting one that **failed** runs it afresh, because a failure is not an answer.
- An expression and a bundle never share a ticket.

### `GET /result/<ticket>`

Only reads: it never dispatches work, so polling is safe.

| body | meaning |
|---|---|
| `pending` | still running |
| `ok <kind> <hash>` | finished, e.g. `ok tree 20e522dfd9fa…` |
| `failed <status> <message>` | the work failed. A server restart mid-run also reads as `failed 500 …; start it again` |

An unknown ticket is `404`; a malformed one is `400`.

### `GET /wait/<ticket>`

Blocks, polling the same state `/result` reports every 100 ms, and answers like
`/eval` does: `<kind> <hash>\n` on success, or the failure the ticket recorded,
with its recorded status code and message as the HTTP status and body (500 if
the recorded status is unusable). Nothing is dispatched, so a client can
`/start`, go away, and `/wait` later or from somewhere else. Unknown and
malformed tickets are `404` and `400`, as for `/result`.

### `GET /status/<ticket>`

The live trace of the ticket (see SPEC "Tracing"). The root node also carries
`origin_ms`, the root request's `requested` time as absolute unix milliseconds;
every other time in the response is relative to it, so a reader that did not
start the run (`caos-cli watch`) can still tell how long ago it began.

## Expression endpoints

An *expression* is one `.caos-expr` line: a verb and the arguments `caos-cli run`
takes. These endpoints let any HTTP client evaluate one.

| endpoint | |
|---|---|
| `POST /start/run?<params>` | start the evaluation; answers `<ticket>\n` at once |
| `GET\|POST /eval/run?<params>` | start it and wait; answers `<kind> <hash>\n` |
| `POST /start/curry?<params>`, `GET\|POST /eval/curry?<params>` | the same for `curry`, which forms the ArgTree and runs nothing |

Only the verbs `run` and `curry` exist. Any other verb is a `400`.

### Parameters

Parameters are `name[:type]=value` pairs joined with `&`, percent-encoded. The
type rides in the parameter's name, exactly as it rides in `caos-cli run
--name:type=value`:

```text
POST /eval/run?base:hash=<oid>&msg=hello
POST /eval/run?base:@@=github%3AMetta-AI%2Fcaos%3Frev%3D<sha>%26dir%3Dstd%2Fllm-step
```

- `salt` is the one reserved parameter. It is not an argument; it only changes
  the ticket and the cache key, to force a fresh run.
- There is no `:@=` type: nothing local can be named by a server. A `:@@=`
  locator is resolved by the server (see SPEC "Evaluating a path").
- An argument given twice, a parameter without `=`, or an unknown type is a
  `400`.
- For a value too large for a URL, send a `POST` body of the same form
  (`application/x-www-form-urlencoded`). It is joined to the query string. In a
  body `+` means a space (so `curl --data-urlencode` works); a literal `+` is
  `%2B`. In the query string `+` is itself.
- Arguments are sorted before use, so two spellings of one call are the same
  call (and the same ticket).

`/start/<verb>` accepts `POST` only; `/eval/<verb>` accepts `GET` and `POST`.

### Answers and errors

A success is `<kind> <hash>` (for example `tree 20e522dfd9fa…`): the type and
object hash of what the line produced. Read it back with `GET /object/<hash>`
or by passing the hash as `in`/`root` to the file tools. For `curry` it is the
formed ArgTree.

Errors are a status code and a one-line text body: `400` for a caller mistake;
for an evaluation that failed, the status the failure recorded (usually `500`).

### Example

```sh
ticket=$(curl -sS -X POST "$server/start/run" \
  --data-urlencode "base:hash=$tool" --data-urlencode "msg=hi")
curl -sS "$server/wait/$ticket"          # tree <oid>
# or, without holding a connection open:
until state=$(curl -sS "$server/result/$ticket"); [ "$state" != pending ]; do sleep 1; done
echo "$state"                           # ok tree <oid>
# or, in one call:
curl -sS -X POST "$server/eval/run" \
  --data-urlencode "base:hash=$tool" --data-urlencode "msg=hi"
```

## Bundled requests: `POST /start` and `POST /submit`

This is what `caos-cli run` sends. The client pushes what only it can read (the
call's args, and the tree a path base is evaluated in) as one Git tree, then
sends a JSON body that names it. Everything that needs evaluating is evaluated by
the server.

```json
{
  "bundle": "<hash of a tree holding `args/` and, for a path base, `root/`>",
  "base":   { "type": "path|locator|hash|docker", "value": "…" },
  "locators": { "<arg name>": "<git locator>" },
  "salt":   "…"
}
```

- `base.type`: `path` is evaluated in `bundle/root`; `locator` is a git locator
  the server fetches; `hash` is an object already in the store; `docker` is an
  image digest. Anything else is a `400`.
- `locators` are extra args the server fetches and adds to the call.

| endpoint | |
|---|---|
| `POST /start` | answers `<ticket>\n` at once; the server forms the request and **runs it** on the ticket's thread. Wait with `GET /wait/<ticket>` |
| `POST /submit` | blocks while the server forms the request, and answers `tree <argtree>\ngranted <secret names>\n`. **Runs nothing** |

Both take the secrets headers and `X-Caos-Git-Token` (below). A malformed body
is a `400`.

## Request headers

Credentials and caller identity are headers, never parameters, because a
parameter becomes part of an ArgTree and so of a cache key.

| header | used by | meaning |
|---|---|---|
| `X-Caos-Secret-Readers` | all compute endpoints | the secret reader keys the caller presents; they decide which secrets the evaluation can use |
| `X-Caos-Conversation` | all compute endpoints | the conversation the secrets are scoped to |
| `X-Caos-Git-Token` | `GET /eval`, `POST /start`, `POST /submit`, `POST /git/import` | a token for one HTTPS Git URL, for private repositories |
| `X-Caos-Step` | `/mcp` | the step that runs tools: a git locator, or a bare 40-hex hash |
| `X-Caos-Max-Result-Chars` | `/mcp` | how long a tool's text may be before it is kept in the conversation and answered with a preview (default 50000; `0` for no limit) |

## `POST /mcp`: the MCP tool server

The built-in MCP server is a thin layer over the expression endpoints. One
JSON-RPC 2.0 message per request, answered in the body (MCP's streamable HTTP
transport without the optional event stream; a notification gets `202` with no
body; a non-`POST` gets `405`). The endpoint keeps no state between messages, so
every request carries `X-Caos-Step` and `X-Caos-Secret-Readers`.

Methods: `initialize`, `tools/list`, `tools/call`, `ping`. Anything else is
JSON-RPC error `-32601`. The tool list is a constant: `run`, `help`, `start`,
`result`, `eval-path`.

Each `tools/call` names its conversation in the `caos_session` argument (the
conversation id is `cc/<session>`) and optionally `caos_tool_use_id`. Both are
supplied by the harness's PreToolUse hook. They map onto the endpoints above as
follows, with the step (`X-Caos-Step`) as the `base` and the arguments
`conversation`, `tools-only`, `tool`, `arguments` (JSON) and `max-result-chars`:

| MCP tool | HTTP equivalent |
|---|---|
| `run` | `POST /eval/run` of the step; blocks |
| `start` | `POST /start/run` of the same call; the answer text carries the ticket |
| `result` | `GET /result/<ticket>` |
| `help`, `eval-path` | `/eval/run` of the step's `tool_help` / `eval_path` |

`start` answers with the ticket at once; the job declares the call and runs it.
The model may end its turn first, which closes nothing the call needs: a
conversation records facts, not turns. The ticket is the same one `run` of that
call would have used, so starting a call that has finished does not run it
again.

`help` with no `path` reports on the connection (conversation, head commit,
diagnostics) and needs no tool run. A tool that cannot run at all is a JSON-RPC
error (`-32603`); one that ran and failed is a result with `isError: true`.

See `rust/crates/caos-cli/MCP.md` for what the tools mean to a model and how
Claude Code is wired to them, and `caos mcp serve` for the stdio bridge used when
a container can only reach the server through `caos://`.

## Other endpoints

Documented elsewhere; listed here so the surface is in one place.

| endpoint | purpose | see |
|---|---|---|
| `GET /run?req=<argtree hash>` | run an ArgTree already in the store | SPEC "Submitting work" |
| `GET /eval?root:hash=<oid>&path=<p>` or `root:@@=<locator>` | evaluate a path under `.caos-expr` | SPEC "Evaluating a path" |
| `GET /status/<ticket or arg tree hash>[?all=1]` | trace of a run | SPEC "Tracing" |
| `GET\|HEAD /object/<hash>`, `POST /object` | content-addressed storage | `design/commits.md` |
| `POST /git/import`, `POST /git/push` | import / publish Git history (`push` takes `allow_ignored`; an ignored-files rejection is a `422` listing the paths) | `design/git-import.md`, `design/agent-publish.md` |
| `/info/refs`, `/git-upload-pack`, `/git-receive-pack` | Git smart-HTTP | `rust/crates/server/src/git.rs` |
| `POST /runner/poll`, `/runner/result`, `/runner/lease`, `POST /sub-run`, `POST /trace/child` | worker protocol | `design/runner-protocol.md` |
| `POST /secrets/push` | secret store updates | SPEC "Secrets" |
