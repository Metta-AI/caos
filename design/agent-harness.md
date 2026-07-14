# Agent harness: conversations as commit chains — design note

**Status:** design agreed, prerequisites in progress (run-then + first-class
commits, on a worktree branch). Builds on map-then (`map-then.md`).

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
- **Step commit** (internal) — one LLM API round + its tool results. Chain of
  steps hangs off the turn. Tree = workspace **plus a reserved top-level
  `.caos/step.json`** holding the raw API request/response blocks for the
  round (verbatim — the API requires replaying assistant blocks, including
  thinking-block signatures, unmodified).
- **Turn commit (agent)** — a **merge**: first parent = the human turn,
  second parent = the last step commit. Tree = final workspace with `.caos/`
  dropped (pure). Message = the response text.

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

`llm-step` worker input: conversation head (commit) + curried
`{api key, model, system prompt, tool registry}`. It fetches the chain lazily,
rebuilds the API request from the step transcripts, POSTs `/v1/messages`, then:

- **No tool calls** → mint the turn merge commit, return `commit <hash>`.
- **Tool calls** → mint a step commit, then execute the calls **serially
  without re-calling the LLM** via run-then: emit
  `{in: <call 1 args>, run: <tool image>, then: curry(self,
  {conversation: <step commit>, pending: [calls 2..N], results: [...]})}`
  and exit. Each continuation pops the next pending call; when the queue is
  empty, all `tool_result` blocks go back in a single user message (required
  by the API) and the next LLM round fires. No dispatcher worker — the step
  worker knows each tool's image. Parallel map-then execution of read-only
  tools is a later optimization.

Tool classes:

- **Tree surgery** (write/edit/mkdir): pure CAS linking, applied inline and
  serially by the step worker. Not sub-runs.
- **Compute tools** (bash, build, test, search): run-then sub-runs. Input
  includes the workspace tree **with `.caos/` stripped** — tools never see
  transcripts, and tool cache keys stay identical to real workspace trees.
  No network in tool images; only the llm-step image has egress.

**The whole tree is never materialized — by any tool, ever.** No FUSE, no
"fetch everything" escape hatch. Every tool is either *bounded* or
*decomposed*:

- **bash** (bounded): `{tree, cmd, paths}` → `{stdout tail, exit code,
  tree}`. The worker recursively fetches only the declared `paths`. A
  command touching an undeclared path hits a placeholder (owner-only,
  worker unprivileged) → loud EACCES; the wrapper turns permission-denied
  paths in stderr into a structured retry hint. Staging the result skips
  placeholders — untouched subtrees round-trip by their recorded hash.
  (No hard materialization budget for now; enforcement can come later if
  models abuse `paths`.)
- **grep** (decomposed): an rgrep worker on the file-count model — on a
  tree, emit `{in, map: curry(self, pattern), then: curry(self, pattern)}`
  and exit; on a blob, fetch just it and grep; in the then position, prefix
  each child's match paths with its child name while folding up (path
  context reassembles level by level). Cached per (subtree hash, pattern):
  after a one-file edit, re-grepping costs only the spine above the edit.
- **ls/listing**: tree objects are names+oids — no content fetch at all.
- **build/test**: the existing caos-native decompositions (rustc,
  deep-deps), not bash.

The model is steered by tool descriptions (a separate grep tool matches the
tool surface models are trained on), the EACCES hint, and the budget error —
all failure modes redirect loudly to the right tool.

**Tool failures are values, not errors.** A failing command (`exit 1`,
compile error) is a normal result — `{exit, stderr tail, tree}` — returned
to the model as a tool result so it can react. Only infrastructure failures
(object fetch failed, container died) error the sub-run and fail the turn.

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
`refs/caos/progress/<conversation>` to the server's existing smart-HTTP
transport after each step; the client watches a turn by polling `git fetch`
on that ref. No new progress API, no enabling change: the server sets
`http.receivepack=true` on its repo at startup (`main.rs`), so push over
smart-HTTP already works. (A stale comment in `git.rs` says otherwise.)

The ref is also what makes step commits *discoverable at all*: until the
turn completes, steps are unreferenced objects known only by hash. And it
doubles as crash insurance — if a turn run fails past retries (e.g. API
errors), the minted steps are orphaned but still reachable via the progress
ref; a retry restarts from the human commit today, but the ref head is a
valid conversation state a future resume could start from.

## Client

`caos-cli chat <conversation>`: create the human commit → request the run →
hang, printing progress from the ref → on completion advance
`refs/caos/conversations/<name>` and print the response. Conversation
identity is that ref — the only mutable thing, owned by the client.

## Caching / retry semantics

LLM steps are nondeterministic and effectively never cache-hit (unique
prompt+tree, plus commit timestamps). This is accepted: retry = new commit
with a new timestamp, no salt machinery needed. Determinism-based requeue
(deadline_ms) can double-call the API; first-post-wins keeps it correct and
the cost is accepted for the prototype. A step is one API round, so per-job
deadlines are comfortable; the top-level pending timeout
(`CAOS_PENDING_TIMEOUT_SECS`) must be generous for whole-turn runs.

## Prerequisites (in progress)

1. **run-then** — single-valued map-then: continuation `{in, map?, run?,
   then?}` (map/run mutually exclusive); `run(--in)` → R; `then(--in,
   --result=R)`; no then → R.
2. **First-class commits** — `commit` storage kind, `commit <hash>` result
   kind (gitlink tree entries), opt-in unpeeled commit args, worker helpers
   to read/write commits.

## Build order after prerequisites

3. bash tool worker
4. llm-step worker (API call, step commits, run-then chaining, turn merge,
   progress ref push)
5. `caos-cli chat`
