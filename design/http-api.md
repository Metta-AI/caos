# HTTP API

The caos server listens on one port and speaks plain HTTP. This page is the
reference for the endpoints a client uses to *drive* caos: evaluate an
expression, start one without waiting, ask how it went, and the MCP tool server
built on top of them. (The storage, Git and runner endpoints are listed at the
end; they have their own documents.)

Source: `rust/crates/server/src/main.rs` (`route`), `compute.rs` ("Expression
lines") and `mcp.rs`. Tests: `tests/expression-http`, `tests/llm-tools-only-http`.

## Expression endpoints

An *expression* is one `.caos-expr` line: a verb and the arguments `caos-cli run`
takes. These endpoints let any HTTP client evaluate one.

| endpoint | |
|---|---|
| `GET\|POST /eval/run?<params>` | evaluate the line and **block** for the answer |
| `GET\|POST /eval/curry?<params>` | form the ArgTree and run nothing; answers with its hash |
| `POST /start/run?<params>` | begin the evaluation and return a **ticket** at once |
| `GET /result/<ticket>` | the state of a started evaluation; only reads |

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
  the cache key, to force a fresh run.
- There is no `:@=` type: nothing local can be named by a server. A `:@@=`
  locator is resolved by the server (see SPEC "Evaluating a path").
- An argument given twice, a parameter without `=`, or an unknown type is a
  `400`.
- For a value too large for a URL, send a `POST` body of the same form
  (`application/x-www-form-urlencoded`). It is joined to the query string. In a
  body `+` means a space (so `curl --data-urlencode` works); a literal `+` is
  `%2B`. In the query string `+` is itself.
- Arguments are sorted before use, so two spellings of one call are the same
  call.

### Answers

`/eval/run` answers `<kind> <hash>\n` (for example `tree 20e522dfd9fa…`): the
type and the object hash of what the line produced. Read it back with
`GET /object/<hash>` or by passing the hash as `in`/`root` to the file tools.
`/eval/curry` answers the same shape for the formed ArgTree.

Errors are the status code and a one-line text body: `400` for a caller mistake,
`500` for an evaluation that failed.

### Tickets

`POST /start/run` answers `<ticket>\n` immediately and evaluates on the server in
the background. The ticket is a 40-hex name derived from the call's content (the
verb, the salt, the arguments and the secret scope), so:

- the same call always has the same ticket;
- starting a call that is already running joins it, and starting one that has
  succeeded returns its ticket without running it again;
- starting one that **failed** runs it afresh, because a failure is not an answer.

`GET /result/<ticket>` answers one of:

| body | meaning |
|---|---|
| `pending` | still running |
| `ok <kind> <hash>` | finished; same value `/eval/run` would have answered |
| `failed <status> <message>` | the evaluation failed. A server restart mid-run also reads as `failed 500 …; start it again` |

Asking never dispatches work, so polling is safe. An unknown ticket is `404`; a
malformed one is `400`. `GET /status/<ticket>` reads the ticket's trace (see
SPEC "Tracing").

### Example

```sh
ticket=$(curl -sS -X POST "$server/start/run" \
  --data-urlencode "base:hash=$tool" --data-urlencode "msg=hi")
until state=$(curl -sS "$server/result/$ticket"); [ "$state" != pending ]; do sleep 1; done
echo "$state"      # ok tree <oid>
```

## Request headers

Credentials and caller identity are headers, never parameters, because a
parameter becomes part of an ArgTree and so of a cache key.

| header | used by | meaning |
|---|---|---|
| `X-Caos-Secret-Readers` | all compute endpoints | the secret reader keys the caller presents; they decide which secrets the evaluation can use |
| `X-Caos-Conversation` | all compute endpoints | the conversation the secrets are scoped to |
| `X-Caos-Git-Token` | `/eval`, `/submit`, `/git/import` | a token for one HTTPS Git URL, for private repositories |
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
| `GET /run?<query>` | run a request given as an ArgTree | SPEC "Submitting work" |
| `POST /submit` | push a bundle holding a call's args and run it | SPEC "Submitting work" |
| `GET /eval?root:hash=<oid>&path=<p>` or `root:@@=<locator>` | evaluate a path under `.caos-expr` | SPEC "Evaluating a path" |
| `GET /status/<arg tree hash>[?all=1]` | trace of a run | SPEC "Tracing" |
| `GET\|HEAD /object/<hash>`, `POST /object` | content-addressed storage | `design/commits.md` |
| `POST /git/import`, `POST /git/push` | import / publish Git history | `design/git-import.md`, `design/agent-publish.md` |
| `/info/refs`, `/git-upload-pack`, `/git-receive-pack` | Git smart-HTTP | `rust/crates/server/src/git.rs` |
| `POST /runner/poll`, `/runner/result`, `/runner/lease`, `POST /sub-run`, `POST /trace/child` | worker protocol | `design/runner-protocol.md` |
| `POST /secrets/push` | secret store updates | SPEC "Secrets" |
