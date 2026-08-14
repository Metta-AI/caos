# Chat: minimal durable model

**Status:** target design; implementation is tracked below.

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

Auxiliary refs hold the title and per-user sidebar membership. They never take
part in turn execution or recovery; `F` is the transcript, workspace, and run
state.

The client and `llm-step` append event commits through an exact-ref CAS;
neither resets `F`. The client creates user commits locally. `llm-step` uploads
commits through `/object`, then asks the server to move only the named ref from
the expected old hash to the new hash. Raw Git pushes remain available for
multi-ref presentation updates, with a server-owned hook enforcing the same
append rule for conversation heads.

Each non-initial event commit has the previous event as first parent. An
ordinary conversation's initial event first-parents the resolved workspace
base; a materialized fork's initial marker first-parents the selected source
event. Its tree is the current workspace plus reserved `.caos` state, and its
JSON message uses the exact stable discriminator `"kind":
"caos-chat-event"`. There is no numeric event-format version. A missing or
unknown `kind` is not interpreted as a legacy event. `author` plus `content`
adds a transcript message; human messages use `author: user` and one
case-sensitive `username` for attribution and display. Clients separately use
that same resolved identity when they write sidebar membership refs. For other
scalar keys, the newest value wins; keyed collections such as async tasks fold
independently by their own identity.

A CAS loser retries on the new head. Text-only events take its tree; workspace
changes use a three-way merge. Clean Git merges may still require event-specific
logical checks.

The client first constructs the queued user event and its `llm-step` request.
It uploads that closed request graph, then atomically admits the message and a
following event that records the request by moving `F` across both commits in
one CAS. The request hash is the run identity. `llm-step` records every durable
step on `F` with that one `request` value; clients only follow the ref. Any
client that observes an admitted queued or running request may submit that same
request again, making dispatch loss and server restarts recoverable without
inventing another run.

Any number of clients may follow the same conversation. The TUI exposes one
user identity, rather than separate `user` and `username` concepts, and uses it
whenever it appends or indexes a conversation. Sidebar refs encode the UTF-8
bytes of that identity as lowercase hexadecimal prefixed with `u-`; the event
keeps the original case-sensitive display value. Usernames are limited to 126
UTF-8 bytes so the encoded directory component remains filesystem-safe. The
conversation ID is independently encoded as lowercase UTF-8 hexadecimal
prefixed with `c-` and limited to 124 UTF-8 bytes, so the encoded terminal
component plus Git's `.lock` suffix fits the filesystem component limit. An ID
therefore remains one reversible component even when its canonical ref contains
`/`.

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
to `complete` or `failed`, then returns `R`'s result unchanged. The conversation
stores neither `R` nor its output. Recovery checks the exact
`refs/caos/res/Q` ref and reissues Q whenever the task's folded state is
terminal but that result is not yet addressable. Reading the completed result
follows `refs/caos/res/Q`; it never reruns Q as a substitute for a read.

### Subagents

A subagent is the same mechanism with a child conversation's `llm-step`
request as `R`. The child advances its own ref while the parent continues.
Completion updates the parent's task status; `Q` returns the child result.
Nothing merges the child workspace into the parent—applying files is explicit.

### Invariants

1. Record an action before starting it and a result before consuming it.
2. Accepted events become reachable immediately and survive client loss.
3. Conversation refs only advance along their first-parent history by CAS.

---

## Implementation

- [ ] one durable event log, CAS appends, recovery, and ordinary tool calls
- [ ] `run-async` client command
- [ ] independent work and `run-and-update-ref`
- [ ] multiplayer following and interjections
- [ ] one user identity
- [ ] `/ref`
- [ ] `/invite`
- [ ] durable `/from` forks
- [ ] detached-task follower after foreground exit
- [ ] child conversations and subagents
- [ ] detached-work execution-context propagation
- [ ] plain-commit and mid-tool-batch fork contract
- [ ] head-keyed folded projection/event cache for long conversations
- [ ] result-ref and server-reflog retention, pruning, and large-installation
  ref scaling

## Model

A conversation is one append-only ref:

```text
refs/caos/conversations/<id>/head -> latest event commit
```

Small title and per-user membership refs support the sidebar. They do not take
part in the conversation append protocol. The title lives at
`refs/caos/conversations/<id>/title`.

Conversation refs live below `refs/caos/conversations/`; sidebar membership
refs live below `refs/caos/users/`. There is no version component in either
path. Compatibility comes from a coordinated destructive development cutover,
not from readers guessing which historical format an unversioned ref contains.
The new reader accepts only commits marked `"kind": "caos-chat-event"` and does
not parse, migrate, rename, or republish an older conversation layout.

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
Closing a client may lose a draft, but not submitted input or remote work.

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
absent. A retry after an ambiguous transport failure first reads `F`: an
observed value equal to the proposed creation tip means the first attempt
succeeded; any other value is a collision and must not be adopted as the new
conversation. Child-conversation creation follows the same create-only rule,
so replaying its recorded creation can recover idempotently without attaching
the child request to somebody else's existing history.

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

When the trees conflict, do not make conflict markers the canonical workspace
and do not discard the losing proposal. Append a terminal conflict event whose
first parent is the current head and whose second parent is the proposed
workspace commit. Its tree remains the clean current tree, while its structured
payload records the base, current and proposed commits and the conflicting
paths. This keeps both versions reachable and leaves an explicit resolution
operation for a later turn. Clean merges still record the event's observed
head and target request/call IDs; consumers check those IDs before acting.

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

A new-turn event `B` sets `status: queued`. That commit plus the fixed
`llm-step` configuration determines an ArgTree request hash `R`; `R` is the run
identity, so there is no separate run ID or `run` field. After uploading the
closed graph for `R`, the client creates the tree-neutral admission event `C`
with `request: R` and CAS-pushes `A -> C`. Although the update crosses two
commits, observers can see neither one without the other. Once claimed, later
events carry the same `request`; `status` says whether it is active or terminal.

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

The client calls `/run?req=R`. CAOS uses `R` as its cache and single-flight key:
concurrent or repeated calls join the same owner for as long as that owner is
alive. Waiting callers never time out and begin duplicate execution. Completed
results are cached and normally pinned under `refs/caos/res/R`. Reissuing a
completed asynchronous task is a cache hit and must not append a new `pending`
state.

CAOS does not yet durably queue in-flight resolution across a server restart.
The admitted request and every completed remote event remain on `head`, so any
following client can issue `R` again and `llm-step` recovers idempotently. A
future durable CAOS work queue can make that reconciliation server-initiated
without changing the conversation model.

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
part of the format. A reader rejects a missing or unknown kind instead of
treating it as an older chat event. Additive fields may be ignored by readers
that do not project them; a truly incompatible event family needs a different
descriptive kind and an explicit reader decision, not a numeric counter.

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
- follow `head` and render its message map;
- own drafts and UI state, but no remote execution state.

**`llm-step`**

- run entirely remotely after launch;
- append model steps, recoverable status, tool calls/results, and the final
  response to `head` as they happen;
- reconstruct all continuation state from history plus ordinary work results;
- notice user commits added while it runs and include them at the next safe
  boundary;
- fold independent-work status separately per task at start and safe
  boundaries; reissue pending tasks, and reissue terminal tasks only while
  their exact result ref is absent;
- normally CAS-update only the named conversation's `head`; when explicitly
  creating a subagent, create that child's initial head as part of the recorded
  parent operation.

**CAOS server**

- remain ignorant of conversation event semantics during compute and ref-update
  admission; startup crash repair is the narrow planned exception, using only
  the stable event discriminator to bound a first-parent integrity walk at the
  ordinary workspace base;
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
  the target before returning the ordinary result.

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
  named ref before passing through the subrequest's result.

No new server endpoint is needed. The server already owns a `/run` request
after receiving it and continues when the HTTP client disconnects. A failed or
lost dispatch remains `pending` on the conversation and is sent again during
recovery. `GET /run?req=Q` is an execution/admission operation, not a result
read: it may run Q when no cached owner or result exists. Consumers retrieve a
terminal result only by resolving and reading the exact
`refs/caos/res/Q` ref, which is side-effect-free. This distinction matters for
caught failures, which may be intentionally uncached and would otherwise be
executed again merely to inspect their payload.

Detached work currently receives only the context encoded in its ArgTree.
Before independent tools or subagents can promise parity with an attached turn,
define and implement an explicit, least-privilege propagation contract for
model settings, credentials, secrets and other execution context. Until then,
callers must treat context-dependent detached work as unsupported rather than
silently inheriting ambient state.

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
   rereads `F` and retries.
5. Finish returns `R`'s result unchanged, so it becomes the ordinary cached
   result of `Q`. No `request` or `out` file is stored in the conversation.

Status events do not touch the workspace, so completion has no file merge.
Duplicate starts identify the same `Q`; CAOS joins, cache-hits, or reruns it as
usual. The failure continuation similarly writes `failed`.

At startup and safe boundaries, recovery handles every folded task, not merely
the last async event:

- a `pending` Q is reissued;
- a `complete` or `failed` Q with an addressable `refs/caos/res/Q` is converged
  and is not reissued;
- a terminal Q without that exact result ref is reissued, closing the crash
  window in which finish appended the terminal event but the server had not yet
  pinned Q's returned result.

Appending the same terminal outcome again is idempotent. A retry of an
uncached caught failure may instead succeed; in that case finish appends a
newer `complete` event for Q, so the per-task fold agrees with the result the
server will pin. A later `llm-step` synthesizes one notice at the durable
terminal event's position and reads the payload through `refs/caos/res/Q`; it
neither appends that notice again nor uses `/run` as a read API.

## Subagents

A subagent uses the same protocol with a child-conversation request as `R`:

1. Create the child's initial event and conversation ref, then build its
   ordinary `llm-step` request `R_child`.
2. Build `Q = run-and-update-ref { subreq: R_child, target-ref: parent F }`,
   append the parent's `pending` status, and call `run-async Q`.
3. The parent immediately receives task ID `Q` and continues. Meanwhile the
   child advances its own conversation ref normally.
4. When the child run returns, `Q` marks the parent task `complete` and passes
   through the child's result. The parent can read `refs/caos/res/Q` or inspect
   the child conversation; no child workspace is merged into the parent.

Multiple children may run concurrently. A `wait(Q)` may reissue Q while
pending, but retrieves an addressable result through the exact
`refs/caos/res/Q` ref. Applying files from that result is a separate explicit
operation.

## Deferred work

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
- Conversation refresh and worker recovery can still rebuild the first-parent
  event spine, and some changed-head paths derive more than one projection from
  that history. Across a long sequence of appends or polls, those repeated
  O(n) walks create an O(n²) cumulative ceiling. Cache the validated event
  suffix and folded projections by canonical head, reusing an ancestor entry
  when the append-only/CAS invariant proves it safe. This is a performance
  follow-up; the current remote ref remains authoritative.
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

1. Every submitted or remote event becomes reachable immediately.
2. Ref updates only append by compare-and-swap; cancel and retry never erase.
3. Every action is recorded before it is launched.
4. Every result is recorded before execution continues from it.
5. No client holds unique recovery state: losing one cannot invalidate accepted
   execution, and any later follower can reissue the recorded request after
   server loss.
