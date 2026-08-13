# Chat v2: minimal durable model

**Status:** from-scratch proposal, not a migration plan.

## Distilled

A conversation is one append-only ref:

```text
F = refs/caos/conversations/<id>/head
```

The client and `llm-step` append event commits by CAS-pushing `old -> new`;
neither resets `F`. The client creates and pushes user commits locally.
`llm-step` uploads commits through `/object`, then moves `F` with an empty-pack
receive-pack push.

An event commit has the previous event as first parent, the current workspace
plus reserved `.caos` state as its tree, and a key/value message. `author` plus
`content` adds a transcript message; for other keys, the newest value wins.

A CAS loser retries on the new head. Text-only events take its tree; workspace
changes use a three-way merge. Clean Git merges may still require event-specific
logical checks.

The client commits a queued user event, then starts the `llm-step` CAOS request.
The request hash is the run identity. `llm-step` records every durable step on
`F`; clients only follow the ref.

### Tool calls

Before running tools, `llm-step` commits the model's complete response,
including ordered `{id, name, args}` calls. Inline tools run directly; compute
tools suspend `llm-step` through ordinary serial `run-then`. `llm-step` commits
each result before the next model call. Recovery repeats the same CAOS request.

### Independent work

For subrequest `R` and target ref `F`:

```text
Q = std/run-and-update-ref { subreq: R, target-ref: F }
task ID = hash(Q)
```

`llm-step` commits `.caos/async/<Q>/status = pending`, calls the generic
nonblocking `/bin/caos run-async Q`, returns `Q` to the model, and continues.

`Q` uses ordinary `run-then` to run `R`. Its finish stage CAS-appends a commit
changing only that task's status to `complete` (or `failed`), then returns `R`'s
result unchanged. The conversation stores neither `R` nor its output: both are
available through `Q`. Cancellation writes `canceled`, which a late finish
does not replace.

### Subagents

A subagent is the same mechanism with a child conversation's `llm-step`
request as `R`. The child advances its own ref while the parent continues.
Completion updates the parent's task status; `Q` returns the child result.
Nothing merges the child workspace into the parent—applying files is explicit.

### Invariants

1. Record an action before starting it and a result before consuming it.
2. Accepted events become reachable immediately and survive client loss.
3. Conversation refs only advance by CAS; the CAOS server remains generic.

---

## Model

A conversation is one append-only ref:

```text
refs/caos/conversations/<id>/head -> latest event commit
```

The client appends user events; the remote `llm-step` appends everything it
does. Neither resets the ref.

Advance `head` after every durable event, not only at the end of a turn.
Closing a client may lose a draft, but not submitted input or remote work.

## Append protocol

To append to a remote head `A`:

1. Create event commit `B` with first parent `A`.
2. Push the objects and the ref update `(expected=A, new=B)`.
3. Git accepts it only if the remote ref is still `A` and `B` descends from
   `A`.

That receive-pack ref command is the compare-and-swap. On rejection, reread
the remote head and retry as below.

The remote ref is authoritative. A client's local ref is only a cache.

### Losing the compare-and-swap

Suppose `B`, based on `A`, loses because `head` has advanced to `C`. For now:

1. Create `B'` with parents `[C, B]` and the same event as `B`.
2. Set its tree by merging tips `C` and `B` using their common base `A`.
3. Push `(expected=C, new=B')`; repeat if that also loses.

This combines two tips, but is called a **three-way merge** because it examines
three trees: base `A`, current head `C`, and proposal `B`. The base identifies
what each side changed; a two-tree comparison cannot reliably distinguish an
edit from an unchanged or deleted path. For a message-only event the trees are
usually identical, so the merge is trivial. Stale candidates remain reachable
through `B'` rather than becoming mysterious loose objects.

A clean tree merge does not imply a logically valid event. Predictable cases
include:

- canceling a run that has already finished or been replaced;
- incorporating a result for a call that is no longer pending;
- continuing a model response after a new user message changed its premise;
- applying two individually valid workspace edits whose combination is wrong.

Initially we can still append the merge, recording the event's observed head
and target request/call IDs. Consumers must check those IDs before acting; later,
event types can define stronger reject, retry, or recompute policies.

### User path

```text
message + working tree
  -> client creates commit locally
  -> git push (objects plus A -> B)
  -> remote head = B
  -> build request R from B
  -> GET /run?req=R if no run is active
```

The client pushes the actual commit; it does not upload a message blob for
another component to translate. For a text-only message, `B` uses `A`'s tree,
not a possibly stale checkout.

If the user also changed a checkout based on commit `P`, the client first
snapshots it as `U` with parent `P`, then merges tips `A` and `U`. This applies
the delta `P..U` to the conversation workspace rather than treating every
difference between `P` and `A` as a user edit. The event has parents `[A, U]`.
If `head` moved to `C`, merge `C` and `U` instead. This is ordinary
`git merge-tree --write-tree`, already used by the client publishing path;
`std/merge` is the remote-worker form of the same operation.

A new-turn event sets `status: queued`. That commit plus the fixed `llm-step`
configuration determines an ArgTree request hash `R`; `R` is the run identity,
so there is no separate run ID. An interjection only appends its message; the
active run notices it.

The client calls `/run?req=R`. CAOS already uses `R` as its cache and
single-flight key, so concurrent or repeated calls are a generic CAOS concern,
not chat state. Completed results are cached and normally pinned under `res/R`.

CAOS does not yet durably queue in-flight resolution across a server restart.
Chat v1 need not solve that: the queued user event and every completed remote
event remain on `head`, so reopening the chat can issue `R` again and
`llm-step` can recover. A future durable CAOS work queue would make restart
automatic without changing the conversation model.

### Remote path

```text
llm-step reads remote head A
  -> stores commit B in the CAOS server's Git object database
  -> receive-pack (empty pack plus A -> B)
  -> remote head = B
```

This is the same server repository that users configure as the `caos` Git
remote. `llm-step` constructs the raw commit and the injected `/bin/caos`
uploads it through `/object`; this is not `/cas/std`. It then updates `head`
through the server's ordinary smart-HTTP receive-pack endpoint. Because the
commit is already in the server's object database, that push needs no objects:
just the `(A -> B)` command and an empty pack.

Thus `llm-step` itself updates the conversation ref. The current worker already
does exactly this for progress refs with a small receive-pack client. Chat v1
can extend that code to `head`; tool workers do not update conversation refs.
A rejected update is rebuilt and retried as above.

## Commits

An event commit has:

- first parent = previous event;
- tree = workspace after the event;
- message = a map, schematically:

```text
author: assistant
content: I fixed the race.
title: Preserve work across disconnects
status: idle
```

If `content` exists, `author` is required and the pair adds one transcript
message. Every other key is conversation state: scan the first-parent history
oldest-to-newest and the latest specified value wins. A null value clears a
key. Thus title, status, model, request hash, and future properties need no
sidecar refs or new commit types.

Tool calls and results are events keyed by call ID, not another waterfall
property. The UI derives current activity from calls without results.

Commits without `content` record internal progress. Only recovery-worthy
progress belongs here; token-level streaming may remain ephemeral.

An extra parent is used only when incorporating genuinely independent history,
such as a mutating tool result or a child conversation. The first-parent log
remains the complete ordered conversation.

## Responsibilities

**Client**

- fetch `head`, create the user's event locally, and CAS-push it;
- start `llm-step` when the submitted event needs a new run;
- follow `head` and render its message map;
- own drafts and UI state, but no remote execution state.

**`llm-step`**

- run entirely remotely after launch;
- append model steps, recoverable status, tool calls/results, and the final
  response to `head` as they happen;
- reconstruct all continuation state from history plus ordinary work results;
- notice user commits added while it runs and include them at the next safe
  boundary;
- CAS-push only the named conversation's `head`.

**CAOS server**

- remain a generic Git/object/ref and durable work service;
- know nothing about conversations.

**Tool worker**

- receive CAS arguments and return an ordinary CAOS result;
- never move a conversation ref.

## Tool calls

An ordinary tool call suspends the conversation:

1. `llm-step` appends the model step containing `{id, name, args}`.
2. Inline tools run in `llm-step`. For a compute tool, `llm-step` yields a
   `run-then`: `run` is the tool worker and `then` is the `llm-step`
   continuation.
3. The server runs the tool and resumes `llm-step` with its result.
4. `llm-step` appends the result before calling the model again.

The worker container is released while the tool runs, but the conversation
does not progress independently. Calls run serially for now.

The recorded call and input workspace determine the same CAOS request on
recovery. Emitting the same `run-then` either joins the in-flight work, returns
its cached result, or runs it.

## Independent work

Independent work uses one generic server primitive and one std worker:

- `/bin/caos run-async Q` ensures request `Q` is running and returns without
  waiting for its result;
- `std/run-and-update-ref` runs a subrequest and, when it finishes, updates a
  named ref before passing through the subrequest's result.

For subrequest `R` and conversation ref `F`, build and put:

```text
Q = std/run-and-update-ref { subreq: R, target-ref: F }
task ID = hash(Q)
```

`Q` contains `R` and `F`, so the conversation need not copy them elsewhere.
The dispatch protocol is:

1. `llm-step` appends a commit changing only
   `.caos/async/<Q>/status` to `pending`.
2. It calls `caos run-async Q`, immediately returns `{task: Q, status:
   pending}` for the model's tool call, and continues the primary conversation.
3. In the background, `run-and-update-ref` emits an ordinary `run-then` with
   `run = R` and a finish stage as `then`.
4. On success, finish reads the latest `F` tip `P`, creates a single-parent
   commit `J` from `P` changing only `.caos/async/<Q>/status` to `complete`, and
   CAS-pushes `P -> J`. A lost CAS rereads `F` and retries.
5. Finish returns `R`'s result unchanged, so it becomes the ordinary cached
   result of `Q`. No `request` or `out` file is stored in the conversation.

Different tasks touch different `<Q>` paths, so completion has no file merge.
Duplicate starts identify the same `Q`; CAOS joins, cache-hits, or reruns it as
usual. A subsequent `llm-step` notices the completion commit and obtains the
result through `Q` (`GET /run?req=Q` or `res/Q`).

The failure continuation similarly writes `failed`. Cancellation writes
`canceled`; a late finish observes that state and does not replace it.

## Subagents

A subagent uses the same protocol with a child-conversation request as `R`:

1. Create the child's initial event and conversation ref, then build its
   ordinary `llm-step` request `R_child`.
2. Build `Q = run-and-update-ref { subreq: R_child, target-ref: parent F }`,
   append the parent's `pending` status, and call `run-async Q`.
3. The parent immediately receives task ID `Q` and continues. Meanwhile the
   child advances its own conversation ref normally.
4. When the child run returns, `Q` marks the parent task `complete` and passes
   through the child's result. The parent can query `Q` or inspect the child
   conversation; no child workspace is merged into the parent.

Multiple children may run concurrently. A `wait(Q)` waits for or retrieves
Q's ordinary result. Applying files from that result is a separate explicit
operation. Canceling a parent does not erase its children; canceling a child is
an explicit event on the child's conversation.

## Invariants

1. Every submitted or remote event becomes reachable immediately.
2. Ref updates only append by compare-and-swap; cancel and retry never erase.
3. Every action is recorded before it is launched.
4. Every result is recorded before execution continues from it.
5. Client presence affects observation, never remote execution or durability.
