# Chat: minimal durable model

**Status:** implemented by this stack except for the items explicitly listed
under Deferred work.

This is a destructive replacement of the earlier chat formats. It deliberately
uses the existing unversioned ref namespace and has no compatibility reader or
migration path. Before switching a development repository to this format,
delete every old ref below both `refs/caos/conversations/` and
`refs/caos/users/` in the client and server repositories. Old commits may be
garbage-collected after their reflogs and any other refs stop retaining them.

For example, from each repository that stores chat refs:

```sh
git for-each-ref --format='delete %(refname)' \
  refs/caos/conversations/ refs/caos/users/ |
  git update-ref --stdin
```

Do not run an old and a new chat binary against the same repository. A rollout
means stopping the old binaries, wiping those ref subtrees, and then starting
only the new binaries.

## Distilled

A conversation is one append-only ref:

```text
F = refs/caos/conversations/<id>/head
```

`F` is the sole authority for transcript, workspace, and foreground and async
run state. Auxiliary refs hold the title and per-user sidebar membership for
presentation and discovery. Initial creation and ambiguous-create recovery
coordinate them atomically with `F`; workers and replay never consult them.

The client and `llm-step` append event commits through an exact-ref CAS;
neither resets `F`. The client creates user commits locally. `llm-step` uploads
commits through `/object`, then asks the server to move only the named ref from
the expected old hash to the new hash. Raw Git pushes remain available for
multi-ref presentation updates, with a server-owned hook enforcing the same
append rule for conversation heads.

The canonical tip must be an event with the exact stable discriminator
`"kind": "caos-chat-event"`. Replay follows first parents while that
discriminator is present; the first non-event parent is the ordinary workspace
base, not chat history. Each event therefore has a first parent: an ordinary
conversation's initial event first-parents the resolved workspace base, while
a materialized fork's initial marker first-parents the selected source event. A
parentless recognized event is malformed. Its tree is the current workspace
plus reserved `.caos` state; ordinary bases and user proposals may not supply a
top-level `.caos` entry. There is no numeric event-format version, compatibility
reader, or interpretation of an unknown kind as an older event. Durable object
and request identities accepted at protocol boundaries are canonical lowercase
40-character hexadecimal Git SHA-1 IDs.
`author` plus `content` adds a transcript message; human messages use `author:
user` and one case-sensitive `username` for attribution and display. Clients
separately use that same resolved identity when they write sidebar membership
refs. For other scalar keys, the newest value wins; keyed collections such as
async tasks fold independently by their own identity.

A CAS loser retries on the new head. Text-only events take its tree; workspace
changes use a three-way merge. Clean Git merges may still require event-specific
logical checks.

The client constructs new-turn user event `B`, derives `llm-step` request `R`
from `B`, uploads that closed request graph, then creates `B`'s direct child
`C` with `{status: "queued", request: R, request_head: B}`. One CAS publishes
both commits. A worker may claim `R` only after proving `B` is on the canonical
spine and its immediate child binds both `R` and `request_head: B`. Creating
`F` also creates its title and the creator's active-membership ref in one atomic
multi-ref Git push with create-only leases, while requiring that user's
archived ref to be absent. Later head appends use exact-ref CAS. The request
hash is the run identity. Foreground lifecycle, model, and tool events carry
that one `request`; interjections deliberately do not take lifecycle ownership,
and per-task async status events are keyed only by task ID. Any client that
observes an admitted queued or running request may submit that same request
again, making dispatch loss and server restarts recoverable without inventing
another run.

Any number of clients may follow the same conversation. The TUI exposes one
user identity, rather than separate `user` and `username` concepts, and uses it
whenever it appends or indexes a conversation. Sidebar refs encode the UTF-8
bytes of that identity as lowercase hexadecimal prefixed with `u-`; the event
keeps the original case-sensitive display value. Usernames are limited to 126
UTF-8 bytes so the encoded directory component remains filesystem-safe. The
conversation ID is independently encoded as lowercase UTF-8 hexadecimal
prefixed with `c-` and limited to 124 UTF-8 bytes, so the encoded terminal
component plus Git's `.lock` suffix fits the filesystem component limit. The
canonical ID itself must obey Git ref grammar and may not create reserved
`head` or `title` path components. An ID therefore remains one reversible
membership component even when its canonical conversation ref contains `/`.

An explicit `--username` wins. Otherwise clients use the normalized `$USER`
(trim surrounding whitespace, preserve case and internal spaces, reject control
and invisible Unicode formatting characters). A line client may fall back to
Git `user.name`, then `user`, when `$USER` is absent; the TUI instead requires
`--username` in that environment. `$USER` intentionally remains ahead of Git
configuration. Until a persisted identity exists, an environment with a shared
or generic value such as `root` or `ubuntu` must pass a personal `--username`
explicitly to avoid merging multiple people's attribution and membership. The
fallback is only a convenience, not a second identity source.

Membership refs use ref-safe keys, not the display values literally. The user
key is `u-<lowercase hex of the normalized username's UTF-8 bytes>`; for
example, `Alice Smith` is `u-416c69636520536d697468`. The conversation key is
`c-<lowercase hex of the conversation ID's UTF-8 bytes>`; for example,
`project/talk-1` is
`c-70726f6a6563742f74616c6b2d31`. Encoding the whole ID prevents the Git ref
file/directory collision that raw IDs `project` and `project/talk-1` would
otherwise create in one membership namespace.

For a given user and conversation, active and archived membership are mutually
exclusive. Invitations preserve an existing archived choice instead of
silently unarchiving it; archive and unarchive move membership with leases on
both refs in one atomic push.

The title ref is mutable presentation state, not execution state. Creation
publishes a deterministic fallback atomically with `F`. A generated title may
replace only the exact fallback it was based on; any manual or foreign rename
wins the compare-and-swap race.

Followers poll the exact head away from the input/render thread and perform a
full coherent load only when that head changes. At most one refresh is in
flight. A result is applied only if its observed head is still relevant, and a
pending local submission remains visible until its durable commit is observed;
stale network results cannot roll back newer conversation or draft state.

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

If Q has no recorded state, `llm-step` commits
`{"kind":"caos-chat-event","async":{"task":"<Q>","status":"pending"}}`,
calls the generic nonblocking
`/bin/caos run-async Q`, returns `Q` to the model, and continues. Repeating the
same tool request folds the existing state for Q and does not append another
`pending` event after Q has reached a terminal state.

`Q` uses `run-request-then R`, preserving R's exact request identity. Its
finish stage CAS-appends a commit with the same tree and that task's status set
to `complete` or `failed`. Success passes `R`'s result through unchanged; a
caught failure returns a small result tree containing `status` and `error` after
recording `failed`. The conversation stores neither `R` nor its output.
Recovery checks the exact `refs/caos/res/Q` ref and reissues Q whenever the
task's folded state is terminal but that result is not yet addressable.

The current model-facing surface synthesizes only a durable terminal-status
notice; it has no `wait(Q)` or result-consumption tool. The exact
`refs/caos/res/Q` ref is currently an addressability and recovery probe, not a
payload the model reads. `/run` is execution, never a substitute for a read.

### Invariants

1. Record an action before starting it and a result before consuming it.
2. Accepted events become reachable immediately and survive client loss.
3. Conversation refs only advance along their first-parent history by CAS.

---

## Implementation in this stack

- [x] one durable event log, CAS appends, recovery, and ordinary tool calls
- [x] `run-async` client command
- [x] independent work and `run-and-update-ref`
- [x] multiplayer following and interjections
- [x] one user identity
- [x] `/ref`
- [x] `/invite`
- [x] durable `/from` forks from conversation events
- [ ] async result consumption or `wait(Q)`
- [ ] detached-task follower after foreground exit
- [ ] child conversations and subagents
- [ ] detached-work execution-context propagation
- [ ] membership policy for line clients joining existing conversations
- [ ] plain-commit and mid-tool-batch fork contract
- [ ] head-keyed folded projection/event cache for long conversations
- [ ] result-ref and server-reflog retention, pruning, and large-installation
  ref scaling
- [ ] one persisted identity default, including environments whose `$USER` is
  absent or shared/generic

## Model

A conversation is one append-only ref:

```text
refs/caos/conversations/<id>/head -> latest event commit
```

Small title and per-user membership refs support the sidebar. Initial creation
coordinates them atomically with the head; after that they do not take part in
the conversation append protocol. The title lives at
`refs/caos/conversations/<id>/title`.

Conversation refs live below `refs/caos/conversations/`; sidebar membership
refs live below `refs/caos/users/`. There is no version component in either
path. Compatibility comes from a coordinated destructive development cutover,
not from readers guessing which historical format an unversioned ref contains.
The canonical tip must be marked `"kind": "caos-chat-event"`; replay follows
recognized events to the first non-event parent and treats that one commit as
the ordinary workspace base. The reader does not parse, migrate, rename, or
republish an older conversation layout.

For a human identity `U`, its ref-safe key is `u-` followed by the lowercase
hexadecimal encoding of U's UTF-8 bytes. This is reversible and collision-free,
preserves spaces and case, and prevents a username from introducing ref path
components. For example, `Ada Lovelace` becomes
`u-416461204c6f76656c616365`. A conversation ID is encoded independently as a
single `c-` plus lowercase UTF-8 hexadecimal component. For example,
`project/talk-1` becomes `c-70726f6a6563742f74616c6b2d31`. Active and archived
membership refs are:

```text
refs/caos/users/<user-key>/conversations/active/<conversation-key>
refs/caos/users/<user-key>/conversations/archived/<conversation-key>
```

The reader accepts only this keyed membership layout and decodes the component
back to the exact conversation ID. It does not read or migrate raw-ID
membership refs; the destructive cutover above removes them.

The server keeps reflogs for crash repair while automatic Git GC is disabled.
Those reflogs do not expire on their own. A bounded retention/pruning policy is
follow-up work and must preserve a documented recovery window; until then,
unbounded reflog growth is an accepted operational cost rather than something
ordinary Git maintenance is assumed to handle.

The client appends user events; the remote `llm-step` appends everything it
does. Neither resets the ref.

Advance `head` after every durable event, not only at the end of a turn.
Closing a client may lose a draft, but not submitted input or already-recorded
remote progress.

## Append protocol

To append to a remote head `A`:

1. Create event commit `B` with first parent `A`.
2. Upload the new object closure.
3. Ask the exact-ref service for `(ref=F, expected=A, new=B)`.
4. The server accepts it only if `F` is still `A` and `A` is on `B`'s
   first-parent history.

That ref command is the compare-and-swap. It returns the observed head on a
race, so the caller retries as below without downloading a repository-wide ref
advertisement.

The remote ref is authoritative. A client's local ref is only a cache. The
server enforces this first-parent append rule for conversation-head refs and
rejects deletion or history replacement. Administrative repair can still use a
local `git update-ref`, outside the network protocol.

Creation uses the same append protocol with an explicit expected-absent value
for canonical `F`. That expected value describes the ref, not the commit's
parentage: an ordinary initial event first-parents the resolved workspace base,
while a materialized fork marker first-parents its selected source event. The
atomic first publication separately creates the title and active membership
under create-only presentation-ref leases while proving archived membership
absent. A retry after an ambiguous transport failure first reads `F`. For
ordinary message creation, an observed tip equal to the proposal—or a descendant
that contains it on the first-parent spine—proves the first attempt succeeded;
any unrelated value is a collision and must not be adopted. Materialized forks
have one narrow additional recovery case: two structurally equivalent markers
can differ only because their commit timestamps produced different object IDs.
The loser may adopt the equivalent marker already on the canonical spine and
create only its missing membership under the active/archived absence leases; it
does not rewrite the canonical head or title. A future child-conversation
protocol must use an equally explicit create-only identity rule so replaying a
recorded creation cannot attach a child request to somebody else's history.

### Losing the compare-and-swap

There is no universal rule that grafts a losing candidate onto the new head. A
failed CAS does not accept or retain that event candidate. The writer rereads
and folds the new head `C`, then applies event-specific reconciliation with
bounded retries (currently 32):

- a tree-neutral message or progress event is rebuilt with `C` as its sole
  first parent;
- an idle user submission rebuilds its user event, exact request, and admission
  event together; if `C` is now active, the submission becomes an interjection
  and reuses the admitted request;
- foreground model and tool work revalidates the request and continuation
  identities, while a terminal event that loses to an interjection resumes the
  active request rather than replaying the stale terminal;
- an async status append refolds that task's state by Q;
- an independently prepared workspace proposal `U`, based on `P`, applies the
  delta `P..U` to `C` with a three-way merge. The resulting event has `C` as
  first parent and retains `U` as a second parent when they differ. It is this
  independent proposal, not the rejected event candidate, that stays reachable.

The workspace operation is called a **three-way merge** because it examines
three trees: proposal base `P`, current head `C`, and proposal `U`. The base
identifies what each side changed; a two-tree comparison cannot reliably
distinguish an edit from an unchanged or deleted path. A clean tree merge still
does not imply a logically valid event: the first parent records the observed
canonical head, while consumers recheck request, round, and call identities
before appending.

When an idle submission that owns the foreground lifecycle finds a tree
conflict, do not make conflict markers the canonical workspace and do not
discard the losing proposal. Append a terminal conflict event whose first
parent is the current head and whose second parent is the proposed workspace
commit. Its tree remains the clean current tree, while its structured payload
records the base, current and proposed commits and the conflicting paths. This
keeps both workspace versions reachable and records the conflict for later
resolution. A conflicting proposal against an already-active request is an
interjection failure instead: reject it without moving `F` or changing that
request's status, and let the rich client restore the draft and show the error.

### User path

```text
message + working tree
  -> client creates commit locally
  -> build request R from B
  -> upload the closed request graph for R
  -> append admission event C containing request R
  -> one CAS-push moves remote head A -> C, making B and C reachable together
  -> GET /run?req=R
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

A new-turn user event `B` plus the fixed `llm-step` configuration determines an
ArgTree request hash `R`; `R` is the run identity, so there is no separate run
ID or `run` field. After uploading the closed graph for `R`, the client creates
the tree-neutral admission event `C` with `request: R`, `request_head: B`, and
`status: queued`, then CAS-pushes `A -> C`. Although the update crosses two
commits, observers can see neither one without the other. Once claimed, later
foreground lifecycle, model, and tool events carry the same `request`; `status`
says whether it is active or terminal.

Lifecycle transitions are scoped to that exact request, not treated as a
conversation-wide boolean. A worker starting `R` refolds the log before
claiming it: if `R` is already terminal it does nothing, and if a newer request
has been admitted the older worker cannot append `running` over it. Admission
also requires `request_head` to be the immediate first parent, preventing a
request from claiming a different user anchor.

An admission CAS retry rebuilds the whole admission candidate. After rereading
the new head, the client creates a new user event `B'`, derives a new
`llm-step` request `R'` from `B'`, uploads the complete graph for `R'`, and
creates its matching admission event `C'`. It must not graft stale `R` onto a
rebased message: the request hash commits to the exact queued head from which
the worker will resume.

If another client wins that CAS, the loser rereads the new state. When a run is
now active it appends its message as an interjection and returns the already
recorded request, so it also reissues the only valid run instead of creating a
second claimant. A client that reloads any admitted `queued` or `running`
request does the same. The active run notices interjections at safe boundaries.

The terminal boundary is also a safe interjection boundary. Before appending a
terminal event, `llm-step` compare-and-swaps the head it last examined. If an
interjection won that race, it must not replay the terminal event on top of the
new input. It rereads and folds the new head, resumes the active request with
the interjection included, and attempts termination only after the resulting
model/tool work reaches another terminal boundary.

The client calls `/run?req=R`. CAOS uses `R` as its cache and single-flight key.
Ordinary concurrent duplicates park behind one in-process owner without timing
out and starting duplicate work. If parking would close a waits-for cycle, that
arrival runs independently with expanded ancestry so ordinary stack-cycle
detection can report the cycle; owner loss wakes its waiters with an error.
After joining or claiming a flight, the dispatcher rereads the best-effort
Redis result cache before doing work, closing the miss-then-owner handoff race.
Completed results are normally also pinned under `refs/caos/res/R`. A terminal
async Q with that exact result ref is not reissued and never appends a new
`pending` state; this addressability check is stronger than assuming every
completed result remains in Redis.

CAOS does not yet durably queue in-flight resolution across a server restart.
The admitted request and every completed remote event remain on `head`, so any
following client can issue `R` again and `llm-step` recovers idempotently. A
future durable CAOS work queue can make that reconciliation server-initiated
without changing the conversation model.

### Materialized forks

`/from` creates a new conversation immediately, before its first new message.
Its source must be a canonical lowercase object ID whose commit message is a
recognized conversation event; plain workspace commits are not accepted. The
new marker has the source event as first parent, the source tree, and
`forked_from` metadata equal to that parent. Readers treat the commit graph as
authoritative and reject a marker whose metadata disagrees with it.

The marker, title, and creator membership are published by the same create-only
transaction used for any new conversation. Replay follows the inherited event
history, while workspace diffing treats the source event as the fork base so it
shows only work added by the fork. A failed asynchronous creation leaves the
local draft in a safe new-conversation placeholder; it never silently creates a
marker-less replacement from the source.

The implemented validity check establishes event ancestry, not a complete turn
boundary. Callers should choose a completed turn. Plain-commit sources,
mid-tool-batch resume semantics, and inherited async-task ownership remain
explicitly unsupported below.

### Remote path

```text
llm-step reads exactly remote head A
  -> stores commit B in the CAOS server's Git object database
  -> exact-ref CAS (F, A -> B)
  -> remote head = B
```

This is the same server repository that users configure as the `caos` Git
remote. `llm-step` constructs the raw commit and the injected `/bin/caos`
uploads it through `/object`; this is not `/cas/std`. It then updates `head`
through the server's narrow ref endpoint. Thus `llm-step` itself updates the
conversation ref without receiving every unrelated ref. A raw smart-HTTP push
to the same namespace is still checked by a pre-receive hook, so clients cannot
bypass append enforcement. A rejected update is rebuilt and retried as above.

## Commits

An event commit has:

- first parent = previous event; an ordinary initial event instead uses the
  resolved workspace base, while an initial fork marker uses its selected
  source event;
- tree = workspace after the event;
- message = one JSON object, schematically:

```json
{
  "kind": "caos-chat-event",
  "author": "assistant",
  "content": "I fixed the race.",
  "title": "Preserve work across disconnects",
  "status": "idle"
}
```

Every event must have the exact string discriminator
`"kind": "caos-chat-event"`. Numeric event versions such as `"v": 2` are not
part of the format. The canonical tip and every commit treated as history must
have that kind; the first non-event first parent is the ordinary workspace base
and ends replay. It is not interpreted as an older chat event. Additive fields
may be ignored by readers that do not project them; a truly incompatible event
family needs a different descriptive kind and an explicit reader decision, not
a numeric counter. Every recognized event must also have a valid first parent;
a parentless root event is malformed rather than an alternate conversation
base. Object IDs read from refs, events, and run requests must use the canonical
lowercase 40-character hexadecimal spelling.

If `content` exists, `author` is required and the pair adds one transcript
message. Scalar conversation state is folded by scanning the first-parent
history oldest-to-newest; the latest specified value wins and null clears a
key. Keyed collections are folded by their own identity: in particular, each
`async.task` has an independent latest status, so an event for Q2 cannot erase
Q1's state. Thus status, model, request hash, and future run properties need no
sidecar state or new commit types. The initial title may be a fallback in the
first event; its mutable value is presentation state outside the turn protocol.

Malformed payloads inside a recognized event are isolated to the projection
they affect where that is safe. For example, an invalid `async` payload is
warned about and ignored while later valid task events remain usable. This
defensive folding is not a compatibility reader: a commit without the exact
event kind is not accepted as conversation history.

Tool calls and results are events keyed by call ID, not another waterfall
property. The UI derives current activity from calls without results.

Commits without `content` record internal progress. Only recovery-worthy
progress belongs here; token-level streaming may remain ephemeral.

An extra parent is used only when incorporating genuinely independent history,
such as a mutating tool result. The first-parent log remains the complete
ordered conversation.

## Responsibilities

**Client**

- fetch `head`, create the user's event and request locally, upload the closed
  request graph, then CAS-admit both the message and request record;
- use one case-sensitive user identity for message attribution and sidebar
  membership, use its `u-<lowercase UTF-8 hex>` key only in ref paths, and use
  one reversible `c-<lowercase UTF-8 hex>` component for each membership's
  conversation ID;
- submit or resubmit the one admitted `llm-step` request;
- poll the exact `head` away from the UI thread, load all projections from one
  coherent spine read only when it changes, and reject stale load results;
- own drafts and UI state, but no remote execution state.

**`llm-step`**

- run entirely remotely after launch;
- append model steps, recoverable status, tool calls/results, and the final
  response to `head` as they happen;
- reconstruct all continuation state from history plus ordinary work results;
- notice user commits added while it runs and include them at the next safe
  boundary;
- fold independent-work status separately per task at entry and foreground
  terminal boundaries; reissue pending tasks, and reissue terminal tasks only
  while their exact result ref is absent;
- CAS-update only the named conversation's `head`.

**CAOS server**

- remain ignorant of conversation event semantics during compute and ref-update
  admission. Current startup repair requires a canonical conversation head to
  be a readable commit with an intact workspace tree; a future chat-aware spine
  repair is the narrow planned exception and would use the stable discriminator
  to stop at the ordinary workspace base;
- enforce append-only first-parent updates for the conversation-head namespace,
  including expected-absent creation;
- provide exact-ref reads and CAS appends so per-event updates do not download a
  repository-wide advertisement;
- keep request and result refs out of broad fetch advertisements, reject client
  writes to server-owned result refs, and keep short-lived request refs bounded.
  Result refs remain visible to receive-pack for now: existing clients use them
  as negotiation bases when a new request refers to an object held only by the
  server.

**Ordinary tool worker**

- never move a conversation ref.

**`run-and-update-ref`**

- receive one validated target ref as part of its request;
- run the recorded subrequest and CAS-append only that task's terminal event to
  the target before returning the unchanged success result or a synthetic
  caught-failure result.

## Followers and presentation state

The canonical head is the only remote execution state, but a responsive client
also has local optimistic state. Each follower permits one remote refresh at a
time. It first performs an exact head read; an unchanged hash is cheap, while a
changed hash triggers one coherent load that derives snapshot, transcript, and
workspace diff from the same validated first-parent walk. The UI applies that
load only if its conversation and observed-head generation are still current.

Every rich-client submission has a pending record until its exact durable
commit is seen. A peer append or an older network response therefore cannot
erase an optimistic row. Interjections use a dedicated submission path that
appends the message without claiming or terminating the active request. Failure
restores the submitted text without replacing a newer draft typed in the
meantime.

Titles and sidebar membership are presentation refs. The first conversation
transaction publishes a deterministic fallback title. Model-backed generation
runs asynchronously and replaces only that exact fallback by compare-and-swap;
an observed manual or foreign rename permanently wins. Presentation-ref churn
after the canonical creation tip is known cannot turn a successful creation
into a reported failure.

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

Independent work uses the existing `/run` endpoint through one new client
command and one std worker:

- `/bin/caos run-async Q` sends the ordinary `GET /run?req=Q` request and
  returns without waiting for its result;
- `std/run-and-update-ref` runs a subrequest and, when it finishes, updates a
  named ref before returning either the unchanged success result or a synthetic
  caught-failure result.

No new server endpoint is needed. The server already owns a `/run` request
after receiving it and continues when the HTTP client disconnects. A failed or
lost dispatch remains `pending` on the conversation and is sent again during
recovery. `GET /run?req=Q` is an execution/admission operation, not a result
read: it may run Q when no cached owner or result exists. The current
model-facing tool reports task/status/hash only and does not load the result
payload. Exact `refs/caos/res/Q` is used as an addressability and recovery probe;
a future result consumer must read that ref rather than use `/run` as a read
operation. This distinction matters for caught failures, which may be
intentionally uncached and would otherwise be executed again merely to inspect
their payload.

Detached work currently receives only the context encoded in its ArgTree, with
a fresh top-level execution stack. It does not inherit credentials, secrets,
model settings, or the parent's cycle ancestry, so detached requests must
independently avoid dependency cycles. Before independent tools or subagents
can promise parity with an attached turn, define and implement an explicit,
least-privilege propagation contract. Until then, callers must treat
context-dependent detached work as unsupported rather than silently inheriting
ambient state.

For subrequest `R` and conversation ref `F`, build and put:

```text
Q = std/run-and-update-ref { subreq: R, target-ref: F }
task ID = hash(Q)
```

`Q` contains `R` and `F`, so the conversation need not copy them elsewhere.
Async state is not one flat newest-value-wins field. Fold the chronological
event stream into a map keyed by Q; the newest valid status for each Q wins
independently. Valid statuses are `pending`, `complete`, and `failed`. A
malformed async payload is warned about and ignored rather than preventing
later valid events from being folded.

The dispatch protocol is:

1. `llm-step` folds Q's state. Only when Q has no recorded status does it
   append a tree-neutral
   `{"kind":"caos-chat-event","async":{"task":"<Q>","status":"pending"}}`
   event; a repeated request for an existing Q returns the folded state without
   resetting it.
2. When the folded state needs dispatch, it calls `caos run-async Q` without
   waiting. The initial call immediately returns `{task: Q, status: pending}`
   for the model's tool call and continues the primary conversation; a repeated
   tool request returns Q's existing folded status.
3. In the background, `run-and-update-ref` emits `run-request-then R` with a
   finish stage as `then`, preserving R's exact request identity rather than
   synthesizing another request around its arguments.
4. On success, finish reads the latest `F` tip `P`, creates a single-parent,
   tree-neutral `complete` event for Q, and CAS-pushes `P -> J`. A lost CAS
   rereads `F` and retries. It then returns `R`'s result unchanged, so that
   result becomes Q's ordinary cached result.
5. The catch path instead appends `failed`, then returns a small result tree
   containing `status` and the caught `error`; the promise protocol cannot ask
   a callback to rethrow. No `request` or output payload is stored in the
   conversation.

Status events do not touch the workspace, so completion has no file merge.
Duplicate starts identify the same `Q`; CAOS joins, cache-hits, or reruns it as
usual.

At `llm-step` entry and foreground terminal boundaries, recovery handles every
folded task, not merely the last async event:

- a `pending` Q is reissued;
- a `complete` or `failed` Q with an addressable `refs/caos/res/Q` is converged
  and is not reissued;
- a terminal Q without that exact result ref is reissued, closing the crash
  window in which finish appended the terminal event but the server had not yet
  pinned Q's returned result.

Appending the same terminal outcome again is idempotent. A retry of an uncached
caught failure may instead succeed; in that case finish appends a newer
`complete` event for Q, so the per-task fold agrees with the result the server
will pin. A later `llm-step` synthesizes one status-only notice at the durable
terminal event's position. The current model-facing tool has no `wait(Q)` or
result-consumption operation.

## Deferred work

- The model can observe a detached task's durable task ID and status, but it
  cannot wait for Q or consume Q's result payload. Add a side-effect-free
  operation that reads exact `refs/caos/res/Q`; do not implement waiting by
  calling the execution endpoint again.
- Child conversations and subagents are not implemented. The intended
  composition may use a child conversation's `llm-step` request as R and
  `run-and-update-ref` to update the parent's Q, but child creation, result
  consumption, and explicitly applying child files all need their own protocol
  and model-facing operations.
- A line-client append to an existing conversation records the participant's
  message but does not add that participant's sidebar membership. This is an
  accepted initial asymmetry: explicit membership remains separate from event
  authorship. A follow-up must choose one policy for both clients—either
  self-invite the resolved line-client identity on append, or make line-client
  selection membership-only.
- Identity defaults are not persisted. A missing `$USER` has an explicit
  fallback, but a shared or generic `$USER` such as `root` still requires a
  personal `--username`; a durable default and its trust boundary are follow-up
  work.
- A detached task can still be `pending` after the foreground `llm-step` has
  appended its terminal event and exited. At that point no worker process owns
  redispatch if the original admission was lost. Add a durable follower at the
  client/server boundary: on open, reconnect, and each newly observed canonical
  head, fold pending tasks, verify that Q names this conversation's canonical
  target ref, and reissue each `(head, Q)` at most once until newer durable state
  appears. Do not keep `llm-step` alive recursively as a waiter; that consumes a
  worker and still provides no durable ownership after process loss. Until the
  follower exists, only a later `llm-step` invocation supplies another recovery
  boundary; merely leaving the conversation open does not.
- A rich-client load now derives all projections from one coherent spine walk,
  but each changed head still walks the entire first-parent history. Across a
  long sequence of changed-head loads, those O(n) walks create an O(n²)
  cumulative ceiling; `llm-step`'s commit cache is only process-local. Cache the
  validated event suffix and folded projections by canonical head, reusing an
  ancestor entry when the append-only/CAS invariant proves it safe. This is a
  performance follow-up; the current remote ref remains authoritative.
- Head polling and changed-head loads run off the input/render thread with at
  most one refresh in flight, but some reload, archive, manual-rename,
  tool-selection, and finish paths still perform synchronous Git or network
  work on the TUI thread. Move those remaining operations behind the same
  asynchronous, generation-checked boundary.
- Startup ref repair validates each conversation tip and its workspace tree but
  does not yet validate the complete event-parent spine. If an earlier event
  object is lost while a newer tip survives, startup therefore leaves a ref
  whose replay will fail instead of rolling it back through the reflog. Add a
  chat-aware repair walk that validates recognized event commits along the
  first-parent spine, stops after validating the first ordinary workspace base,
  and shares work across refs. A generic recursive parent walk is not suitable:
  it would turn startup repair into a scan of unrelated repository history.
- `/from` materializes conversation event commits, while startup `--from` and
  the earlier virtual flow have also accepted ordinary workspace
  commits. Unify those entry points only after defining the empty-transcript
  semantics for a plain commit. The same contract must identify safe turn
  boundaries: a fork in the middle of a recorded tool-call batch inherits an
  incomplete protocol prefix and cannot yet be promised to resume. It must also
  define whether keyed async task state is inherited, reset, or translated. In
  particular, an inherited pending Q still names the source conversation ref,
  so the fork cannot safely redispatch it against the fork ref; inherited
  terminal notices have a different, read-only meaning. These are known
  limitations, not part of the initial implementation in this stack.
- Exact reads and appends remove repository-wide discovery from the chat event
  path, and short-lived request refs are swept. Result refs still appear in
  receive-pack advertisements because request commits may refer to result
  objects that exist only on the server; hiding them makes Git try and fail to
  repack those absent objects locally. A later protocol should provide a compact
  negotiation anchor (or make clients hydrate every referenced closure), then
  hide and eventually sweep old result refs without breaking cached request
  lookup. Server reflogs used for crash repair also remain unbounded while
  automatic Git GC is disabled; define a bounded pruning policy that preserves
  a documented recovery window. Conversation, title, and membership refs may
  also need sharding once their aggregate advertisement becomes material.

## Invariants

1. Every accepted event becomes reachable at its CAS durability boundary.
2. Ref updates only append by compare-and-swap; retry never erases.
3. Every action is recorded before it is launched.
4. Every result is recorded before execution continues from it.
5. No originating client holds unique recovery state for an admitted foreground
   request: losing it cannot invalidate accepted execution, and a later client
   can reissue the recorded request after server loss. Detached Q redispatch
   after foreground exit is the documented exception until its follower exists.
