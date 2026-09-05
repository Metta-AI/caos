# Chat v3: conversation and code as separate histories

**Status:** implemented through subagents and workspace PR publication.
Selected solely by the `refs/caos/v3/` namespace; v2 refs stay untouched
and invisible. The binding definition is the code:
`rust/crates/conversation-protocol/src/v3/` (records, kinds, paths,
validation) and its golden fixture (`fixtures.rs`), which pins the bytes.

## Conversation, workspace, and publication roles

| Plane | Meaning | Where |
| --- | --- | --- |
| `C` | conversation | commits under `refs/caos/v3/conversations/<key>/head` |
| `W` | workspace code | ordinary git history, named by sha from `C` |
| `P` | publication | the selected `W` commit at `refs/heads/caos/<conversation-id>` under the current `preserve` policy |

A `C` commit is `tree` = the complete conversation state, `parent` = the
previous `C` (the source `C` for a fork, the fixed v3 genesis commit
`G3` for a `conversation.root`),
`message` = the transition kind, one word. No `C` parents a code commit:
the two DAGs are joined only by shas written inside `C`'s tree.

Invariants:

1. Every `C` has exactly one parent; its tree holds `.caos/` and, when
   nonempty, `files/`; `git log` never enters code history.
2. Every change is explained: the kind fixes the delta; a no-op is invalid.
3. Code is named by sha in `C`'s records and fetched by oid. Nothing is
   kept alive by a parent edge; gc protection is not built (the server
   collects nothing yet).
4. `W` and `P` are code roles: no `G3` ancestry, never parsed as
   conversation trees. Ordinary code commits need no format conversion.
   The host currently rejects workspace inputs containing reserved `.caos`
   entries other than `.caos/conflicts`; publication rejects any `.caos`
   entry. These are host restrictions in addition to the protocol's records.
5. Refs only name heads: a conversation or subagent head, a membership,
   a published branch. Everything else is an exact sha, resolved before
   it enters a content-addressed key.
6. Publication leaves workspace pointers unchanged. It appends publication
   records to `C` and updates the remote published branch; it rewrites no
   conversation history. `preserve` creates no additional code commits.
7. The transcript is append-only.

## Refs and keys

```
refs/caos/v3/conversations/<hex(id)>/head
refs/caos/v3/users/<hex(user)>/conversations/{active,archived}/<hex(id)>
refs/heads/caos/<id>                       (publication, on origin)
```

Heads advance only by exact-oid leased CAS (concurrency control, not
validation). A membership ref's existence is the fact; its value is a
stale-tolerant hint. The server is conversation-agnostic
(`client-owned-conversation-refs.md`); repair treats `refs/caos/v3/` as
target-only and never rewinds it through the reflog.

## The tree

```
.caos/format                  "3"
.caos/identity.json           id, kind, optional owner (parent/head/request/round/tool for a child)
.caos/title
.caos/workspaces/<name>/{commit,initial[,origin]}   bare shas
.caos/transcript/<shard>/<ordinal>-<message-id>.json  + payload dir
.caos/requests/<id>.json      .caos/requests/active
.caos/tools/<request>/<round>/<call-id>.json  + payload dir
.caos/async/  .caos/subagents/  .caos/publications/
files/                        conversation-owned files
```

Records are canonical JSON (sorted keys, integers only, LF); protocol
ids are `SHA-256("caos-v3-id\0" || tag || 0 || canonical)`.

The protocol accepts an explicit workspace map, including zero or multiple
workspaces. The host currently creates one: the TUI defaults to `main` at
the local default branch tip; the CLI defaults to `HEAD`, or uses `--base`.
With an explicit base its workspace is named `main`; otherwise the CLI
uses the default branch name, the current branch name, or `main` as a
fallback. The selected code commit is adopted without rewriting it.

## Host and launcher inputs

Separate histories do not yet make launching independent of the checkout.
The host resolves `DEEP-DEPS/llm-step` and `DEEP-DEPS/llm-call` by evaluating
its local tracked working tree. In this repository the root `.caos-expr`
expands the root `DEPS` declarations into those mounts. The checkout's
`.caos-secrets/` store grants the model key to those paths; absent readers
grant nothing. The host also
snapshots local `main`/`master` refs for the merge tool and publishes through
the checkout's `origin` remote.

These inputs come from the host checkout, which may differ from the selected
workspace commit. An independent launcher supplying harness, secret, and
repository inputs remains a followup.

## Transitions

Twenty-three kinds, each a one-word commit message:

```
conversation.root  conversation.fork  metadata.title.set
message.append     request.admit  request.claim  request.interject
request.escape     request.terminal   model.complete
tool.start         tool.complete      files.apply
workspace.create   workspace.rollback workspace.remove
async.start        async.terminal
subagent.spawn     subagent.terminal  subagent.apply
publication.pending  publication.terminal
```

A turn: the client appends the user entry and admits a request in one
leased push (creation also writes the active membership ref, with the
archived one proven absent). The worker claims it, calls the model from
the exact head, records `model.complete`, handles the tool calls, then
records `request.terminal`. Dispatched workspace tools first append
`tool.start` pinning their task and input workspace, then `tool.complete` with
the result. Inline tools and immediate errors complete directly without a
`tool.start`. Conversation-file calls have no workspace target. For inline
file tools, an explicit workspace makes every path workspace-relative;
without one, the `files/` prefix selects conversation-owned files.

Interjections and escapes are records the worker drains at its next
boundary. Async tasks and subagents have start/spawn records on the
parent's spine and a relay appends their terminal records. A subagent has
its own `conversation.root` parenting `G3`, with owner metadata naming the
parent's exact head. It receives the selected workspace, if any, rather
than inheriting the parent's transcript or files. Its workspace result
reaches the parent only through `subagent.apply`.

**Reconciliation.** A missing workspace pointer produces a conflict.
Otherwise the proposal must descend from its declared base, and the
following cases are tried in order:

- Already applied: the proposal equals its base or the current commit,
  is an ancestor of the current commit, or has the same tree as current.
- Direct: current equals the base, or current descends from the base and
  the proposal descends from current.
- Merged: a three-way merge using the declared base succeeds; mint a
  commit with current and proposal as its two parents.
- Conflict: record the candidate and any conflicting paths, leaving the
  workspace pointer unchanged.

The same table serves dispatched tool proposals, subagent application,
and the host's manual tree update. `/update-tree` commits outstanding local
edits, includes already-committed edits, and uses the merge base of local
`HEAD` and the selected workspace as the proposal base. It refuses unrelated
histories or multiple merge bases before staging files.

**Publication.** `preserve` policy only: push the named workspace's
commit to `refs/heads/caos/<id>` on the host checkout's `origin`. Selection
is required when there is more than one workspace; they currently share
that one destination branch. The existing remote tip must be an ancestor
of the selected commit. A leased push then protects against subsequent
remote changes, bracketed by `publication.pending` and
`publication.terminal` (complete, conflict, or uncertain).

Before retrying, the host reconciles pending records for the same repository
and ref using the observed remote tip: the planned head means complete;
an unchanged expected tip means uncertain; another tip means conflict.
It records these outcomes without replaying the old pushes.

The TUI also supports PR publication through `Ctrl+P` (or **Publish pull
request** in its command palette). The user confirms a base, defaulting to
`origin`'s advertised default branch. The host fetches its current commit and
runs an ordinary conversation turn to merge it into the selected workspace
when necessary, then build and test. This preparation can advance `W`; the
subsequent publication operation still leaves workspace pointers unchanged.
Before publishing, the host rejects interruption, missing base ancestry,
remaining conflict markers, reserved `.caos` state, or a workspace changed
since preparation. It uses the same leased branch publisher, then `gh` to
find an open PR for that repository, head, and base or create one.
`/publish-branch` skips preparation and PR creation. Squash, per-workspace
destinations, multi-workspace PR stacks, and conversation publication remain
deferred.

**Forks, title, membership.** `conversation.fork` creates a new identity
with the source `C` as its parent; it is not a `conversation.root`.
The source must be quiescent (no active or cancelling request, no started
tool, no pending async task or publication). The fork inherits its state
with every running child record dropped. A title update changes only the
title file and commits through CAS on the conversation head; conditional
title updates also compare the expected title. Archiving moves the
membership ref.

## Validation

A reader accepts a `C` by structural checks (one parent, registered
kind, root-parents-`G3` and only-root-parents-`G3`, no-op, tree escapes,
non-blob modes, `format`, `title`, canonical bytes of every changed
record) and then by reconstruction: the transition is rebuilt from the
changed records, re-applied to the parent tree in memory, and the
resulting tree oid must equal the child's. `validate_spine` walks a head
back to its root at `G3` or an already-validated boundary. It adds newly
validated commits to the cache only after the whole walk succeeds, so a
failed validation cannot certify a descendant. Workers validate on load
and when adopting remote advances; ordinary appends use the shared
transition application without re-validating the resulting spine.

## Known shortcomings

- No gc protection for code shas named only from `C` (invariant 3).
- No retention or erasure policy; reflogs are kept forever.
- Publication uses one branch per conversation; no squash policy or multi-workspace PR stacks.
- Stacks and surfaces above one conversation are not designed.
