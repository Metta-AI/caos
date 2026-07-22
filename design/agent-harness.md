# Agent harness: conversations as commit chains — design note

**Status:** steps 1–5 implemented — run-then + first-class commits, the
bounded bash tool (`crates/worker-bash-tool`), the llm-step driver
(`crates/worker-llm-step`), and the chat client (`caos-cli chat`,
`crates/caos/src/chat.rs`). Builds on map-then (`map-then.md`). Where this
note and the code diverged during implementation, the note has been updated
to match the code; the deltas are called out inline.

## Idea

Each turn of a conversation (a human update, or an agent response) is a git
commit: message = the turn's text, tree = the workspace after the turn,
parent = the previous turn (or the base commit the conversation started from).
The commit DAG *is* the conversation store: branching a conversation is a
commit with the same parent, retrying is a new commit with a new timestamp,
and `git log` / `git diff` are the transcript and review UI.

Why it fits caos:

- map-then is already an agent loop: a worker either computes a value or
  describes the remaining work and exits. An LLM step is exactly that — call
  the API, and either the turn is done or there are tool calls to run.
- Tool calls become sandboxed caos sub-runs on the workspace tree.
- No cache hits are expected on LLM steps (prompt + tree are effectively
  unique), so commits' timestamp nondeterminism costs nothing. Tool runs on
  identical trees do cache.

## Commit structure

Two kinds of commits, one conversation spine:

- **Human turn** — created client-side, no run needed. Parent = conversation
  head; tree = workspace (with any human edits); message = the human's text.
- **Step commit** (internal) — one LLM API round. The chain of steps hangs
  off the human turn (step 1's parent is the human commit; each next step's
  parent the previous step). Tree = workspace **plus a reserved top-level
  `.caos/step.json`** (format below) holding that round's verbatim response
  blocks — the API requires replaying assistant blocks, including
  thinking-block signatures, unmodified — plus the `tool_result` blocks the
  round's *request* carried.
- **Turn commit (agent)** — a **merge**: first parent = the human turn,
  second parent = the last step commit — or, for a turn that used no tools at
  all, a plain single-parent commit (no steps exist). Tree = final workspace
  with `.caos/` dropped (pure). Message = the response text.

Step and turn commits are authored **`caos-agent <caos@caos>`** with real
wall-clock timestamps (so a retried turn is a distinct commit); that author
name is also how the transcript walk recognizes an agent turn — walking the
first-parent spine down from a human turn, a `caos-agent` parent is the
previous turn's merge and anything else is the conversation's base commit.

**`.caos/step.json`** (defined here; the tests pin it):

```json
{
  "content": [ ...this round's response content blocks, verbatim... ],
  "results": [ ...the tool_result blocks this round's REQUEST carried... ],
  "v": 1
}
```

`results` answers the *previous* step's tool calls (`[]` for a turn's first
round) — a step commit is minted the moment its API round returns, before its
own calls have run, so their results land in the *next* step. To keep the
last round's results and final response blocks tree-reachable, the end_turn
round of a turn that used tools **also mints a (final) step commit**; the
turn merge's second parent is that step. (A delta from the first draft, which
minted steps only for tool-call rounds and would have dropped the last
tool_results from the tree.)

So `git log --first-parent` is the clean conversation; following second
parents gives the full transcript with stock git tooling. All step data is
tree-reachable, so a plain fetch of the conversation head transfers every
workspace state and every API exchange (a blob oid named only in a commit
message would be unreachable — messages are text, not references).

Diff shapes: step↔step = real edits + one modified `.caos/step.json`;
turn↔step = real edits + one deletion; turn↔turn = pure. `.caos` is a
reserved name — the harness errors at conversation start if the base tree
already contains one.

## The step loop

`llm-step` worker arguments (as implemented; the worker itself ships as
`curry(runner, bin=worker-llm-step)` in the shared runner pool, not as its
own image):

- `head:commit=` — the human-turn commit to answer (the conversation head);
- curried config: `api_key`, `system` (the system prompt), `bash_image` (the
  tool registry — just bash for now, an image ref), and optionally `model`
  (default `claude-opus-4-8`), `base_url` (default
  `https://api.anthropic.com`; tests point it at a stub), `conversation`
  (names the progress ref);
- continuation state, curried by the worker itself between tool calls:
  `step:commit=` (the newest step commit), `pending` / `results` (JSON arrays
  of the remaining `tool_use` blocks and the collected `tool_result` blocks),
  `current_id` (the in-flight call's id) — plus the `in`/`result` args the
  run-then resolution itself supplies.

It fetches the chain lazily, rebuilds the API request from the step
transcripts (each prior agent turn replays as its steps' `results`/`content`
messages; a toolless turn as its message text), POSTs `/v1/messages`, then:

- **No tool calls** (`stop_reason: end_turn`) → mint the final step commit
  (only if the turn used tools — see "Commit structure"), then the turn
  commit; `commit <hash>` is the run's result.
- **Tool calls** → mint a step commit, then execute the calls **serially
  without re-calling the LLM** via run-then: emit
  `{in: {tree, cmd, paths}, run: <bash image>, then: curry(self,
  {step: <step commit>, pending: [calls 2..N], results: [...], …config})}`
  and exit. Each continuation pops the next pending call; when the queue is
  empty, all `tool_result` blocks go back in a single user message (required
  by the API) and the next LLM round fires. A non-zero exit marks its
  tool_result `is_error`. No dispatcher worker — the step worker knows each
  tool's image. Parallel map-then execution of read-only tools is a later
  optimization.
- Any other `stop_reason` (`max_tokens`, refusal, …) → the run errors with a
  clear message; the turn fails (prototype behavior).

Tool classes:

- **Inline tools** (implemented — `read`, `ls`, `write`, `edit`;
  `worker-llm-step/src/tools.rs`): hash-level workspace operations the step
  worker executes in-process — no sub-run, no container. Reads materialize
  only the path they touch (bounded: 100KB / offset+limit; `ls` reads one
  tree level); writes/edits rebuild the tree by symlinking every untouched
  entry and `caos put`ting the result (mkdir is implicit). One call queue
  drives both classes serially (`drive`): inline calls advance the workspace
  in-process, a bash call tail-exits into its sub-run, and an
  inline-tools-only round costs zero containers. Failures (missing file,
  non-unique `old_string`) are `is_error` tool_results, not errors. Parameter
  shapes mirror Claude Code's file tools, which models know well.
- **Compute tools** (bash, build, test, search): run-then sub-runs. Input
  includes the workspace tree **with `.caos/` stripped** — tools never see
  transcripts, and tool cache keys stay identical to real workspace trees.
  No network in tool images; only the llm-step image has egress. (Not yet
  enforced: both workers currently run as `curry(runner, bin)` in the shared
  runner pool, whose containers all sit on the compute network — a per-image
  egress fence is future work.)

**The whole tree is never materialized — by any tool, ever.** No FUSE, no
"fetch everything" escape hatch. Every tool is either *bounded* or
*decomposed*:

- **bash** (bounded, `crates/worker-bash-tool`): input `{tree, cmd, paths}`
  (one `in` tree from run-then, or direct `--tree`/`--cmd`/`--paths` args;
  `paths` newline-separated) → a result tree
  `{exit, stdout, stderr, denied?, tree}`: the exit code (decimal;
  128+signal), 100KB tails of both streams, and the staged workspace. The
  worker fetches each declared path's ancestors one level and the leaf
  recursively, then runs `cmd` via `/bin/sh -c` in a *mirror* of the
  workspace: loaded content as writable copies, every undeclared entry a
  symlink to its owner-only `/cas` placeholder (worker unprivileged) → loud
  EACCES; permission-denied paths found in stderr that resolve through a
  placeholder come back in `denied`, one per line — the structured "retry
  with them in `paths`" hint. Staging resolves the placeholder symlinks back
  to their recorded hashes, so untouched subtrees round-trip without a read.
  (No hard materialization budget for now; enforcement can come later if
  models abuse `paths`. Known limitation: CAS blobs materialize without
  git's exec bit, so declared files round-trip as mode 100644.)
- **grep** (decomposed; implemented — `crates/worker-rgrep`, the `grep`
  tool): an rgrep worker on the file-count model, one job per directory. It
  greps the files at its own level, map-thens over a synthetic tree of just
  the subdirectories (map = curry(self, pattern) — the pattern re-curried
  because curry layers unwrap into args), and the then-combiner links the
  local match files and each non-empty child result tree into one **sparse
  result tree**: only matching files appear, each holding `linenum:line`
  matches, children embedded *by hash* — nothing is copied as results ride
  up, results are git-diffable, and identical subtrees share one cached job.
  Cached per (subtree hash, pattern): after a one-file edit, re-grepping
  costs only the spine above the edit, and a scoped grep of `src/` IS the
  cached `src/` node of the full grep. Flattening to `path:linenum:line` is
  the caller's presentation choice — llm-step renders it at the transcript
  boundary (100KB budget, then matching-file counts + a narrow-the-scope
  hint); the pattern is validated in llm-step BEFORE the sub-run launches,
  so a bad regex is an is_error tool_result, never a failed turn. A grep
  result is not a workspace: the pre-grep workspace rides the continuation
  curry, and only bash results advance the tree. `tests/rgrep` drives the
  fold directly (sparse shape, binary skipping, file scope, empty tree,
  cache hit); the LLM integration is covered in `tests/chat-offline`.
- **ls/listing**: tree objects are names+oids — no content fetch at all.
- **build/test**: the existing caos-native decompositions (rustc,
  deep-deps), not bash.

The model is steered by tool descriptions (a separate grep tool matches the
tool surface models are trained on), the EACCES hint, and the budget error —
all failure modes redirect loudly to the right tool.

**Tool failures are values, not errors.** A failing command (`exit 1`,
compile error) is a normal result — the same `{exit, stdout, stderr, tree}`
shape — returned to the model as a tool result (marked `is_error`) so it can
react. Only infrastructure failures (object fetch failed, container died)
error the sub-run and fail the turn.

## LLM API

Raw `POST /v1/messages` (no SDK — none exists for Rust, and the Agent SDK's
job is owning the loop, which caos owns here). `minreq` with `https-rustls`
(matches the workspace's pure-Rust static-musl constraints) + `serde_json`.
One blocking POST per step; no streaming (progress granularity is the step
commit). Hand-rolled retry on 429/5xx honoring `retry-after`. Top-level
`cache_control: {"type": "ephemeral"}` on every request — each step replays
an identical prefix, so steps after the first read the prompt cache.

Secrets: API key curried into the llm-step worker's args. It lives in CAS
and rides in job payloads; the runner-token auth is the fence.

## Progress

The step chain grows in real time. The step worker pushes
`refs/caos/conversations/<name>-progress` (next to the conversation head, so a conversation's refs sit together) to the server's existing smart-HTTP
transport after each step (when a `conversation` arg names one); the client
watches a turn by polling `git fetch` on that ref. No new progress API, no
enabling change: the server sets `http.receivepack=true` on its repo at
startup (`main.rs`), so push over smart-HTTP already works. (A stale comment
in `git.rs` says otherwise.) The worker image carries no `git`, so llm-step
speaks the minimal receive-pack dialect itself (`progress.rs`): read the old
value from the ref advertisement, send one update command plus the constant
*empty* packfile — every object the ref needs already reached the server via
`/object` as the worker built it. The push is best-effort: a failure warns
and the turn continues (observability, not correctness).

The ref is also what makes step commits *discoverable at all*: until the
turn completes, steps are unreferenced objects known only by hash. And it
doubles as crash insurance — if a turn run fails past retries (e.g. API
errors), the minted steps are orphaned but still reachable via the progress
ref; a retry restarts from the human commit today, but the ref head is a
valid conversation state a future resume could start from.

**In-round status** (finer than the step): the API call is the one slow,
silent part of a turn — a toolless turn mints no step until it's over, and a
rate-limited round sleeps invisibly. So the worker also force-updates
`refs/caos/conversations/<name>-status` around each API attempt with a blob
`"<human hash>\n<text>"` — `calling <model>…`, `<why> — retrying in Ns
(attempt M/4)`, `<model> answered in X.Xs` — over the same hand-rolled push
(the blob goes up via `/object` first). The first line scopes the status to
its turn, so a client can ignore a previous turn's leftover. The client polls
it in the same 2s loop and prints changes to *stderr* (transient meta, not
conversation content). Same best-effort contract as the progress ref.

## Client

Two verbs and a full-screen client over one turn engine (implemented —
`crates/caos/src/chat.rs`, tested end-to-end against the stub in
`tests/chat-offline`):

- **`caos talk [<prompt>]`** — the everyday surface. The positional argument
  is the prompt; the conversation is the repo's most recently advanced one
  (`refs/caos/conversations/*` by committer date), `-c <name>` picks one, and
  `--new` mints a fresh auto-named `talk-<n>`. The chosen conversation is
  announced on stderr. With no prompt on a terminal it loops — one turn per
  line, ctrl-d ends, a failed turn is reported and the loop continues (the
  ref didn't advance, so the next line retries from the same head); with
  piped stdin the whole of it is one prompt.
- **`caos-cli chat <name> [-m <message>]`** — the explicit, scriptable
  one-turn form (message from `-m` or stdin).
- **`caos-tui [-c <name> | --new]`** — a Ratatui/Crossterm client in its own
  crate. It consumes structured `TurnEvent`s from the same engine, reconstructs
  durable history from the conversation ref, shows live status/tool activity,
  accepts multiline prompts, and renders the accumulated base-to-head
  workspace diff. Applying that virtual diff takes an explicit double
  `Ctrl+A`, requires a clean host checkout, and first runs `git apply --check`;
  merely opening the TUI never mutates the checkout. Progress remains one
  completed API round at a time, and a running turn is not cancellable until
  the server/runner protocol grows cancellation.

A turn creates the human commit → requests the run → hangs, printing progress
from the ref → on completion advances `refs/caos/conversations/<name>` (in
the *local* repo) and prints the response text and short hash. Conversation
identity is that ref — the only mutable thing, owned by the client. Shared
flags: `--base <revspec>` (a new conversation's base commit, default `HEAD` —
refused if its tree carries a top-level `.caos`), `--system <text>` /
`--system-file <path>` (default: a short coding-agent prompt), `--model`,
`--base-url`, and `--log` (print the conversation so far — the first-parent
walk — and run nothing). The API key comes only from `$ANTHROPIC_API_KEY`
(checked before anything is minted).

The workers come ready-made from the published library: `/cas/std/bash-tool`
and `/cas/std/llm-step` are `curry(runner, bin=<static binary>)` nodes
(published by build-builtins.sh next to the images), so a turn needs nothing
built or committed locally — the bash curry rides as a literal image ref and
the per-turn state (key, system, model…) is curried onto the llm-step curry
(layers flatten). `--llm-step-bin`/`--bash-tool-bin` (or
`$CAOS_LLM_STEP_BIN`/`$CAOS_BASH_TOOL_BIN`) override with a git-tracked local
binary curried onto `/cas/std/runner` — the stub tests' path.

Implementation notes (what the workers assume): the human commit is an
ordinary git commit (any author *except* `caos-agent` — enforced) whose
message is the user's text, first parent the previous turn (or the base),
tree the parent's tree (human turns are text-only for now); the run is
`llm-step` with `--head:commit=<that commit>` plus the curried config above
(the `:commit=` machinery pushes the commit's closure). While the run blocks
— on its own thread; `/run` returns `commit <hash>` directly — the client
polls the progress ref every 2s (`ls-remote` for the tip, the `/object` API
for the step commits, so nothing mid-turn lands in the local repo) and prints
each new step's text blocks and `$ <cmd>` tool-call lines; a chain that roots
at some other human commit is a stale ref and prints nothing. On success the
turn commit is fetched (bringing the whole step chain — it's tree-reachable;
negotiated with the human commit as the sole tip, so the pack is this turn's
new objects — a no-negotiation fetch re-downloads the base's entire history
every turn, ~10s of index-pack CPU on a large repo, while a full multi-ref
negotiation can go multi-round, which the smart-HTTP delegate has been seen
to break on) and the ref advances; on failure the error prints and the ref
is untouched —
the human commit is harmlessly orphaned. `tests/chat-online` runs one tiny real-API
turn as part of the regular suite (self-skipped unless `ANTHROPIC_API_KEY` is
set — the only check the scripted stub can't make).

## Caching / retry semantics

LLM steps are nondeterministic and effectively never cache-hit (unique
prompt+tree, plus commit timestamps). This is accepted: retry = new commit
with a new timestamp, no salt machinery needed. Determinism-based requeue
(deadline_ms) can double-call the API; first-post-wins keeps it correct and
the cost is accepted for the prototype. A step is one API round, so per-job
deadlines are comfortable; the top-level pending timeout
(`CAOS_PENDING_TIMEOUT_SECS`) must be generous for whole-turn runs.

## Build order

1. **run-then** — single-valued map-then: continuation `{in, map?, run?,
   then?}` (map/run mutually exclusive); `run(--in)` → R; `then(--in,
   --result=R)`; no then → R. **Done** (`tests/run-then`).
2. **First-class commits** — `commit` storage kind, `commit <hash>` result
   kind (gitlink tree entries), opt-in unpeeled commit args, worker helpers
   to read/write commits. **Done** (`tests/commit`).
3. bash tool worker. **Done** (`crates/worker-bash-tool`,
   `tests/bash-tool`).
4. llm-step worker (API call, step commits, run-then chaining, turn merge,
   progress ref push). **Done** (`crates/worker-llm-step`,
   `tests/llm-step` — end-to-end against a scripted stub API).
5. `caos-cli chat` (human commits, conversation ref, progress printing).
   **Done** (`crates/caos/src/chat.rs`, `tests/chat-offline`; real-API turn:
   `tests/chat-online`).
6. `caos talk` + std-published worker curries — prompt-first surface, sticky
   conversation, interactive loop; `std/bash-tool` and `std/llm-step`
   published by build-builtins.sh so a turn needs nothing built or committed
   locally. **Done** (same files; `tests/chat-online` is the UX spec).
7. Structured client events + `caos-tui` — presentation-independent turn
   events, durable history/diff readers, multiline composer, task switching,
   live activity, workspace review, and confirmed clean-checkout apply. **Done**
   (`crates/caos-tui`; unit tests plus the existing chat integration suite).
