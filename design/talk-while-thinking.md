# Talk while thinking — spec

> **Historical context only.** The authoritative interjection and append model
> is [`chat.md`](chat.md). The four-ref layout and migrations below describe the
> superseded prototype and must not be implemented as a compatibility reader;
> current clients use isolated v2 refs and leave this old unversioned data
> untouched and invisible.

Steer and cancel a running turn. Extends `agent-harness.md`; terms from there
(turn, step, human commit `H`, turn merge `M`, `caos-agent`).

## Status

- **Superseded protocol.** The four-ref/`from-user` design below is retained as
  history, not as the current specification. Current chat uses one append-only
  event ref; see `chat.md` for the normative ref, commit, CAS, recovery, and
  ownership rules.
- **Interjections are built in.** They append user events to that one spine
  and are consumed at safe model boundaries. Cancellation remains future work.
  The sections below explain the discarded branch-and-merge proposal and must
  not be used to infer current ref names or lifecycle behavior.

## Model

- A turn is two branches from `H` that merge:
  - **agent branch** — step chain `S1..Sn`, one commit per LLM round.
  - **user branch** — `H`, then interjection commits.
  - **turn merge `M`** — `parents = [user tip, Sn]`.
- Each step is itself a merge: `Si.parents = [S(i-1) or H, user tip at the
  round]`, so a step absorbs the user's latest.

```
     Hi1 ─────────── Hi2                (user branch)
    ╱             ╲      ╲
H ← S1 ← S2(⊕Hi1) ← S3(⊕Hi2) ← … ← M    (agent branch; M seals the turn)
```

## Constraint

- An LLM round (one upstream Anthropic `POST /v1/messages`, the sole use of
  that endpoint — caos itself exposes no message POST) is atomic; the runner has
  no cancellation.
- ∴ interjection and cancellation act at **round boundaries**, never mid-round.
  Latency bound: one round + its tool calls.

## Refs — `refs/caos/conversations/<id>/`

| | user side — client writes | agent side — worker writes |
|---|---|---|
| **branch tip** (DAG) | `from-user` | `from-agent` |
| **sidecar blob** | `title` | `status` |

- `from-user` — user branch tip; the client is its sole writer. One ref, three
  phases: **head** (idle, tip = last `M`), **live** (mid-turn, tip = `H`),
  **inject** (advancing as interjections land).
- `from-agent` — agent branch tip (step chain). Today's `-progress`.
- `status` — in-round telemetry blob `"<H>\n<text>"`; not a DAG commit (updated
  without a commit), so it cannot fold into `from-agent`.
- Conversation-name validation (`validated_refname`, chat.rs — runs
  `git check-ref-format` and rejects reserved names) SHALL reject an id whose
  final segment is a channel name (`from-user`/`from-agent`/`title`/`status`),
  replacing today's `-progress`/`-status` suffix check.
- Migration: retire bare `<id>`, `<id>/head`, `<id>-progress`, `<id>-status`;
  `list_conversations` = "every `<id>/from-user`, strip suffix". Touches
  `progress.rs`, `chat.rs`, `agent-harness.md`, tests. Existing conversations
  migrate lazily and idempotently:
  - **Local** (`migrate_legacy_conversation_refs`, at the client entry points):
    a legacy bare head `refs/caos/conversations/<id>` is renamed to
    `<id>/from-user` (delete the bare ref — a file where `<id>/from-user` needs
    a directory — then create the channel).
  - **Server** (`migrate_server_conversation_head`, on first list): a
    conversation an older TUI published only at `<id>/head` is renamed to
    `<id>/from-user` by one atomic push (create the channel, delete the legacy
    ref) the first time `list_user_conversations` sees it.

## Delivery

- Content flows as git objects/refs. The only compute trigger is one
  `GET /run?req=<hash>` per turn (start or restart).
- Interjection = client pushes `from-user`. **No notification.**
- Cancellation = client resets `from-user`. **No notification.**
- Rounds are server-driven (`run-then` promise resolution re-invokes the
  worker); the client does not trigger per round.
- The worker reads `from-user` once per round (ref advertisement + `/object`).
  Nothing to wake mid-round, so a push suffices — no message payload, no new
  endpoint.

## Client

- Owns `from-user`; never writes `from-agent`/`status`.
- Turn start: mint `H` (author ≠ `caos-agent`; parent/tree per **User commit**),
  push, set `from-user=H`, `GET /run`.
- Interjection: mint the user commit, push, advance `from-user`.
- Cancel: reset `from-user` — to the previous `M` (abandon) or a new `H'`
  (restart; new `GET /run`). A failed turn is an abandon.
- Success: advance `from-user` to `M`.
- Poll loop: watch `from-agent` + `status`; read stdin (interactive) / composer
  event (TUI) to interject.

### User commit

- Parent SHALL be the agent tree the message was based on (merge-base
  correctness: else the diff shows the agent's whole step, and its merge-base is
  wrong).
- **Text only**: parent = agent's current step tip; tree = that step's tree →
  empty diff. SHALL NOT snapshot a stale host checkout.
- **Host edits** (`/update-tree`): parent = the commit checked out; tree =
  working tree → diff is exactly the edit.

## Worker (llm-step) — per round

Round boundaries: `start`, `drive`-drain (queue empty → next LLM round),
`end_turn` pre-merge. At each:

1. Read `from-user`.
2. **Cancel check**: if `H` is not an ancestor of the `from-user` tip → stop:
   no LLM call, no merge, exit failure `"turn superseded"`. Minted steps stay
   reachable via `from-agent`.
3. **Interjection**: new = user commits reachable from the `from-user` tip but
   not from the last step's 2nd parent. Fold oldest-first as user messages. (The
   walk in 2–3 is one walk.)
4. Call the LLM.
   - `tool_use` → mint step `Si = merge[prev, from-user tip]`; run tools; loop.
   - `end_turn` → re-run 1–3; if new interjections, continue; else mint `M`,
     finish.

## Commits

- **Step**: author `caos-agent` wall-clock; parents `[prev, from-user tip]`;
  tree = workspace + `.caos/step.json` (format unchanged). Text steering: tree =
  workspace unchanged. Host edits: 3-way merge of the user tree into the
  agent's, base = the agent step the user commit is parented on.
- **Turn merge `M`**: author `caos-agent`; parents `[from-user tip, Sn]`; tree =
  final workspace; message = response text.
- Interjection text rides the user commit's message, not `step.json`.

## Transcript

- `prior_messages`/`step_chain`: first-parent walk, unchanged.
- Replaying a step: emit its 2nd-parent user commit's message as a `user`
  message before the step's assistant blocks.
- `--log`: same 2nd-parent glance → interjections show as human turns.

## Cancellation — three layers

- **L1 abandon-and-restart** — client-only; ignores the orphaned result. Works
  today, no worker/server change.
- **L2 self-stop** — the worker's `from-user` ancestry check halts the chain at
  the next boundary. This note.
- **L3 preemptive** — kill the in-flight round; needs runner cancellation.
  Future.
- L1 makes cancel *correct*, L2 *cheaper*, L3 *faster*. Correctness never
  depends on a race: an `M` minted just before the worker notices is discarded
  by the moved-on client.
- Restart parent: previous `M` (discard partial work) or the agent's last step
  (keep it, redirect). Same gesture, different parent.

## Properties

- **Prompt cache**: interjections append to the message tail; the cached prefix
  is preserved.
- **Impurity**: reading `from-user` makes a step non-pure in its `reqHash` —
  accepted, as today: llm-step steps are nondeterministic, never
  cache-relied-upon, first-post-wins.

## Stages

0. **Refs + migration — DONE.** Four-ref scheme (`from-user`/`from-agent`/
   `title`/`status`); `validated_refname` rejects ids whose final segment is a
   channel; `list_conversations` selects `<id>/from-user`; local
   (`migrate_legacy_conversation_refs`, at `cli_chat`/`cli_talk`/
   `publish_unindexed_conversations`) and server
   (`migrate_server_conversation_head`, in `list_user_conversations`) migration.
   Tests: `legacy_bare_conversation_refs_migrate_to_from_user`,
   `legacy_server_conversation_head_migrates_on_list`, plus the updated
   `chat-offline`/`chat-talk`/`chat-tools*`/`chat-online`/`llm-*`
   integration suites.
1. **Text steering — TODO.** Client interjection (stdin/composer → mint user
   commit → advance `from-user`); `from-user=H` at turn start; worker per-round
   read + fold; step-as-merge (`[prev, from-user tip]`); `end_turn` re-check;
   transcript 2nd-parent replay. Test `tests/talk-inject`: inject between rounds
   and during the final round; `--log` shows interjections.
2. **Cancellation L1–L2 — TODO** (with 1) — client resets `from-user`; worker
   self-stop via the ancestry check. Test: cancel mid-chain → `from-user` rests
   at previous `M`; restart on last step carries partial work forward.
3. **Concurrent host edits — TODO** — user commit carries the working tree; step
   tree = 3-way merge (needs an in-worker merge — `worker-common` or a
   `caos merge-tree`, the image has no `git`); conflicts as a tool-result value.
   Test the merge flow.
4. **Cancellation L3 — TODO** — preemptive kill, with runner cancellation.

## Out of scope

- Mid-round interruption (needs runner cancellation).
- Hierarchical conversation ids (would need a reserved channel sub-segment).
