# Chat: durable conversation log

**Status:** implemented except for the items under Deferred work.

Chat v2 is selected solely by the `refs/caos/v2/` namespace. Events contain no
version field. Existing unversioned chat refs remain untouched and invisible to
the new UI: there is no migration, compatibility reader, or requirement to
delete old data.

## Distilled

A conversation is one append-only ref:

```text
refs/caos/v2/conversations/<id>/head
```

Its first-parent history is the ordered event log, and each event's tree is the
workspace at that point. This ref is the only authority for transcript and run
state. Titles and per-user sidebar membership are separate presentation state;
workers and replay never consult them.

Every event message is a JSON object. Every event has a first parent, including
the oldest event: that root records `base`, equal to its first parent, and replay
stops there. A fork instead first-parents its source event, records the same
commit as `forked_from`, and inherits the source's root. It never creates a
second `base`. Readers validate the event spine through that explicit boundary
and reject a missing or inconsistent root rather than guessing where ordinary
Git history begins. Writers validate new events before appending them, and
readers fail loudly if an invalid boundary reaches the log.

Writers advance the head with an exact compare-and-swap (CAS): ordinary
`git push --force-with-lease` names the head they observed. Commit messages,
trees, and the append-only event discipline are opaque to the server.
A losing writer reloads the canonical head and reconciles according to the
event: text-only events can be rebuilt on the new tip, while independent
workspace changes require a three-way merge. There is deliberately no generic
"force the losing commit on top" rule.

A foreground turn is admitted as two commits published by one head update. The
user event `B` plus the chosen `llm-step` configuration determines request hash
`R`; its immediate child `C` records `request: R`, `request_head: B`, and
`status: queued`. `R` is the sole run identity. A worker can claim it only after
verifying that exact relationship on the canonical spine. Interjections append
to the same history without acquiring lifecycle ownership. Any follower that
sees an admitted queued or running request can safely submit `R` again, so
dispatch loss and server restart do not require unique client state.

Model steps, tool calls, results, and lifecycle changes are events on that same
log. Ordinary tools suspend the turn and are serial: the call is recorded before
execution, and its result before the next model step. Independent work has a
deterministic wrapper request `Q`; repeating `run_async` reads its durable status
and eventual ordinary CAOS result.

Three invariants organize the design:

1. Record an action before launching it and a result before consuming it.
2. Accepted events become reachable immediately and survive client loss.
3. Conversation heads advance only along first-parent history by CAS.

## With details

### Ref layout and compatibility boundary

The canonical and auxiliary refs are:

```text
refs/caos/v2/conversations/<id>/head
refs/caos/v2/conversations/<id>/title
refs/caos/v2/users/<user-key>/conversations/active/<conversation-key>
refs/caos/v2/users/<user-key>/conversations/archived/<conversation-key>
```

Only this namespace participates in discovery, following, and writes. Clients
assign conversation meaning and append-only discipline to these ordinary Git
refs; the server does not. Clients coordinate title and membership refs with
atomic leases. The reader does not parse, rename, import, or republish the old
unversioned layout.

The head is durable execution state. Title and membership refs support the UI
only. A single case-sensitive user identity is used for message attribution and
membership. Canonical head and title refs embed the validated conversation ID
directly; membership ref paths use reversible, lowercase hexadecimal encodings
of user and conversation IDs so arbitrary display values cannot introduce path
components or file/directory collisions. Active and archived membership for
the same user and conversation are mutually exclusive. The particular UI
commands and environment fallbacks used to select an identity are outside this
design.

### Event and replay contract

An event commit has:

- first parent = the preceding event, except that the root first-parents the
  ordinary workspace base and a fork marker first-parents its source event;
- tree = the workspace after the event, plus reserved `.caos` state;
- message = one JSON object describing one or more compatible projections.

A root user event is schematically:

```json
{
  "base": "0123456789abcdef0123456789abcdef01234567",
  "author": "user",
  "username": "Ada",
  "content": "Fix the race."
}
```

There is no `v` field. Durable object and request IDs accepted at protocol
boundaries are canonical lowercase 40-character Git SHA-1 values. Ordinary
workspace bases and user proposals may not introduce the reserved top-level
`.caos` path.

Replay walks the first-parent event spine through its first `base` event, whose
value must equal that event's first parent; ancestry below it is ordinary
workspace history and is not interpreted as events. A parentless event, a
non-object message before the boundary, a missing boundary, or a boundary that
disagrees with its parent makes the conversation malformed. The append protocol
rejects any later `base`, preserving the one-root invariant. A fork marker must
have `forked_from` equal to its first parent and must not carry `base`.

The first parent gives total event order. An extra parent is used only to keep
independently prepared workspace history reachable, such as a user's worktree
proposal or a mutating tool result. It does not create another event stream.

Projections fold oldest to newest. `author` plus `content` adds a transcript
message. For scalar state, the newest specified value wins and null clears it.
Keyed state folds independently: an async event for task `Q2` cannot erase
`Q1`. Tool activity is identified by `(request, round, tool_use_id)`, since a
model's call ID is only round-local. Invalid optional projection payloads may be
ignored where isolation is safe, but a malformed event envelope or history
boundary fails the conversation rather than falling back to an older format.

### Creation, append, and conflicts

To append event `B` to canonical head `A` at ref `F`:

1. Build `B` with first parent `A`.
2. Upload its closed object graph.
3. Run `git push --force-with-lease=F:A B:F`.
4. The client validates the event envelopes and first-parent relationship; Git
   advances `F` only if it is still `A`.

The remote ref is authoritative; a local ref is only a cache. The server accepts
ordinary Git reset and deletion operations: append-only history is a client
protocol invariant, not storage policy. Conversation writers enforce that an
update extends the observed first-parent spine and that an update to an existing
head introduces neither `base` nor `forked_from`; replay validates that boundary
again.
If an append response is lost, finding the candidate at the observed tip or on
its first-parent spine proves success and prevents a duplicate append.
Transcript and run semantics remain client/worker validation, not server
storage policy.

Creation uses an expected-absent head. One atomic, create-only push publishes
the initial head, deterministic fallback title, and creator's active membership
while proving the archived membership absent. After an ambiguous transport
failure, the client reads the canonical head: its proposal or a descendant
proves success, while unrelated history is a name collision. Presentation-ref
churn after the head is known to exist cannot turn successful creation into a
reported failure.

A failed CAS is reconciled by event meaning:

| Candidate | Reconciliation after reloading the head |
| --- | --- |
| Tree-neutral message or progress | Rebuild it on the new tip. |
| New foreground turn | Rebuild its user event, request, and admission together; if a run is now active, append an interjection instead. |
| Foreground worker event | Revalidate the exact request and continuation before appending. |
| Async status | Refold and update only that task `Q`. |
| Workspace proposal based on `P` | Three-way merge the `P..proposal` delta into the new tip and retain the proposal as an extra parent. |

A clean Git tree merge does not by itself make an event logically valid;
request, round, and call identities are rechecked. If a new foreground
submission has a tree conflict, the canonical tree remains clean and a terminal
conflict event records the base, current and proposed commits and paths while
retaining the proposal as a second parent. A conflicting interjection is
rejected without changing the active request, and the client restores its text.
A mutating-tool conflict likewise keeps the current tree, retains the proposal
as a second parent, and records a failed tool result for the owning request.

### Foreground turns and recovery

For an idle conversation, submission follows this path:

```text
message and workspace
  -> user event B
  -> llm-step request R derived from B and its configuration
  -> upload R's closed request graph
  -> admission event C, the immediate child of B
  -> one CAS publishes B and C
  -> submit R to CAOS
```

The admission event binds `{request: R, request_head: B, status: queued}`. Its
request graph is durable before the admission becomes reachable. Although the
ref update crosses two commits, observers see neither without the other. If
that CAS loses, the client must rebuild `B`, derive a new `R`, and build its
matching `C`; a request commits to its exact starting event and cannot be
grafted onto a rebased message.

Foreground lifecycle, model, and tool events carry `R`. Before claiming work,
the worker refolds the canonical log and verifies that `B` is present and its
immediate child admits both `R` and `request_head: B`. A terminal request is not
revived, and a stale worker cannot append `running` over a newer request.
Admission begins at `queued`; the owner appends `running`, then a request-scoped
terminal `idle` or `failed` event, which clears the active request projection.

An interjection appends a user message without claiming or terminating the
active request; execution remains attached to the already admitted `R` and
joins or reissues it only as needed. The worker incorporates such messages at
safe boundaries. If an interjection wins the CAS immediately before a terminal
event, the worker refolds and continues `R`; it does not replay the stale
terminal event over new input.

CAOS uses the request hash as its cache and single-flight identity. The chat
protocol does not assume that an in-flight run survives server restart. It does
ensure that its admission and completed progress survive: any follower that
observes admitted `queued` or `running` state can resubmit the same request, and
`llm-step` reconstructs its continuation from the log and ordinary tool
results.

### Interrupts

Escape is an ordinary tree-neutral event scoped to `R`. A worker that loses CAS
reads the commits after its attempted base; an intervening Escape preserves its
result, closes unrun calls, and ends `R` idle without another model step.

### Workspace changes and forks

For a checkout based on `P`, the client snapshots the proposal as `U` and
three-way merges `P`, the current conversation tree, and `U`. The event's first
parent is the canonical head and `U` is retained as a second parent when the
histories differ. This applies only the user's `P..U` delta instead of treating
all changes since `P` as user edits.

PR publication is an ordinary turn: the agent calls `merge` with the exact
fetched base, resolves and tests. A clean tree without `.caos` becomes the PR
snapshot.

`/from` materializes a new conversation before its first new message. Its
source must be a recognized conversation event whose complete inherited event
spine validates through a matching explicit root. The marker uses the source
event as both first parent and `forked_from`, inherits the source tree and root
boundary, and is published atomically with its title and creator membership.
Replay therefore includes the inherited transcript, while workspace diffing can
treat the source as the fork point. If concurrent creators produce equivalent
markers with different timestamp-derived commit IDs, the loser may adopt the
canonical marker and create only its missing membership; it never rewrites the
head or title. Plain workspace commits, mid-tool-batch resume semantics, and
inherited async-task ownership are not defined yet.

### Ordinary tools

Before running tools, `llm-step` validates that each tool-use ID is a string and
is unique within that model response, then records the response and its ordered
`{id, name, args}` calls. Inline tools run directly. A compute tool suspends the
turn through an ordinary serial `run-then`; the worker result resumes
`llm-step`, which records it before making another model call. Calls are serial
for now.

The recorded call and input workspace determine the same CAOS request during
recovery. Re-emitting it joins in-flight work, uses a cached result, or reruns it
without inventing another chat identity. An ordinary tool worker never updates
the conversation ref; only `llm-step` records its result.

### Independent work

For subrequest `R` and target conversation ref `F`:

```text
Q = std/run-and-update-ref { subreq: R, target-ref: F }
task ID = hash(Q)
```

`Q` contains both the work and its destination, and is the task identity. When
no state exists for `Q`, `llm-step` first appends
`{"async":{"task":"<Q>","status":"pending"}}`, then calls `caos run-async Q`
without waiting. Repeating the same tool request refolds `Q` and does not reset
a terminal task to pending.

`run-and-update-ref` executes the exact subrequest `R`. Its finish stage first
makes the result addressable, then CAS-appends a tree-neutral `complete` or
`failed` event containing both `Q` and that result object ID, retrying on a new
canonical head. Success returns `R`'s ordinary result unchanged; a caught
failure returns the same small failure result named by its event. The
conversation stores neither the subrequest nor its output payload, only its
content address.

At `llm-step` entry and foreground terminal boundaries, recovery considers each
task independently. A pending `Q` is reissued. A terminal event is already
converged because it carries the exact result object ID and is appended only
after that object exists; it never needs a separate result-ref lookup. If
concurrent executions publish different terminal outcomes, the newest terminal
event for `Q` wins. Before any redispatch, `llm-step` opens `Q`, validates its
subrequest, and proves its recorded `target-ref` is this conversation's `F`.

`spawn_agent` atomically creates an owner-indexed child conversation on a clean
snapshot descended from the parent workspace. Its transcript starts at its own
root, while the shared workspace ancestry lets ordinary `merge` apply its
result. Its prompt supplies the normal fallback title, and the sidebar groups it
under the parent recorded by `spawned_by`. Parent call/result and child
prompt/parent link are durable events; the child runs through `run_async`.

### Following and presentation state

A follower refreshes membership, heads, and titles away from the input/render
thread, with at most one refresh in flight. A changed selected head triggers one
coherent spine load from which the snapshot, transcript, and workspace diff are
derived. The result is applied only if its conversation and observed-head
generation are still current, so stale network work cannot roll back the UI.

Each local submission remains optimistically visible until its exact durable
commit appears. Peer appends cannot erase it. On failure, the client restores
the submitted text only if doing so will not replace a newer draft.

Activity is refolded from the durable log whenever the head changes. The client
retains only its selection and scroll position across that refresh.

The initial transaction creates a deterministic fallback title. Asynchronous
title generation may replace only that exact value by CAS; a manual or foreign
rename wins. Membership changes similarly use atomic leases so active and
archived state cannot both win.

The client passes an explicit model to every `llm-step`. `/model` changes the
client's last-used model for later turns. Assistant events retain the model for
display. Escape interrupts a running request; otherwise it focuses the
conversation list.

### Deferred work

- **Async recovery.** Add a durable follower that redispatches pending detached
  tasks after the foreground `llm-step` exits; today only a later invocation
  provides another recovery boundary.
- **Fork contract.** Decide how plain commits create an empty transcript, which
  event positions are safe turn boundaries, and whether inherited async tasks
  are reset, translated, or treated as read-only. A pending task currently names
  the source conversation and cannot safely be redispatched by its fork.
- **Client policy and responsiveness.** Persist a trusted default identity and
  choose a single membership policy for line-client appends. Move the remaining
  synchronous Git/network paths off the TUI thread and separate frequent
  exact-head following from broader sidebar discovery.
- **Long histories.** Cache validated event suffixes and folded projections by
  canonical head instead of rebuilding the full spine after every change.
- **Retention and scale.** Define bounded retention for result refs and server
  reflogs without breaking request-object negotiation or the documented repair
  window. Shard presentation refs if aggregate advertisement size becomes
  material.
