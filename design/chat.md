# Chat v3: conversation and code as separate histories

**Status:** implemented through subagents, workspace navigation, and publication plans.
Selected solely by the `refs/caos/v3/` namespace; v2 refs stay untouched
and invisible. The binding definition is the code:
`rust/crates/conversation-protocol/src/v3/` (records, kinds, paths,
validation) and its golden fixture (`fixtures.rs`), which pins the bytes.

## Conversation, workspace, and publication roles

| Plane | Meaning | Where |
| --- | --- | --- |
| `C` | conversation | commits under `refs/caos/v3/conversations/<key>/head` |
| `W` | workspace code | ordinary git history, named by sha from `C` |
| `P` | publication | a named `W` commit on its configured repository and branch, under the current `preserve` policy |

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
   conversation trees. Ordinary code commits need no format conversion;
   their extra Git headers (including signatures) are preserved. Conversation
   commits have only tree, parent, author, and committer headers.
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
refs/heads/caos/<id>                       (legacy/single-workspace default)
refs/heads/caos-workspaces/<id>/<workspace>  (new multi-workspace defaults)
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
.caos/workspaces/<name>/config.json             optional repository, branch, and base
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

Twenty-four kinds, each a one-word commit message:

```
conversation.root  conversation.fork  metadata.title.set
message.append     request.admit  request.claim  request.interject
request.escape     request.terminal   model.complete
tool.start         tool.complete      files.apply
workspace.create   workspace.configure workspace.rollback workspace.remove
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
  or is an ancestor of the current commit.
- Direct: current equals the base, or current descends from the base and
  the proposal descends from current.
- Merged: a three-way merge using the declared base succeeds; mint a
  commit with current and proposal as its two parents.
- Conflict: record the candidate and any conflicting paths, leaving the
  workspace pointer unchanged.

Tree equality alone does not mean already applied: a commit can add merge
ancestry without changing any files. Such proposals still advance or merge
according to the table. Readers continue to accept older already-applied
records with a retained candidate.

The same table serves dispatched tool proposals, subagent application,
and the host's manual tree update. `/update-tree` commits outstanding local
edits, includes already-committed edits, and uses the merge base of local
`HEAD` and the selected workspace as the proposal base. It refuses unrelated
histories or multiple merge bases before staging files.

**Workspace settings.** Optional `config.json` records a repository URL,
publication branch, and base. A base names either a repository branch or another
workspace, together with the exact commit last integrated. `workspace.configure`
updates only these settings; it never moves a workspace pointer. Workspace
base edges must name existing workspaces, stay within a repository, and be
acyclic. A referenced base cannot be removed until its dependents are
reconfigured. Missing settings retain the existing checkout-based defaults.
These records supply the common model for workspace navigation, publication
plans, stack updates, and repository attachment.

**Publication.** `preserve` policy pushes a named workspace's commit to its
repository and branch. Each workspace has a distinct destination. Existing
publication records retain their branch; new multi-workspace branches use
`caos-workspaces/<id>/<workspace>` so they can coexist with a legacy `caos/<id>`
branch. A workspace's settings can override its destination. The existing remote
tip must be an ancestor of the selected commit. A leased push protects against
subsequent remote changes, bracketed by `publication.pending` and
`publication.terminal` (complete, conflict, or uncertain).

Before retrying, the host reconciles pending records for the same repository
and ref using the observed remote tip: the planned head means complete;
an unchanged expected tip means uncertain; another tip means conflict.
It records these outcomes without replaying the old pushes.

`Ctrl+P` opens a publication preview, initially selecting the current workspace.
Space selects individual rows; `a` toggles all. `b` edits a PR base (a branch or
`@workspace`), and `h` edits the published branch. Enter or a second Ctrl+P
confirms. The preview loads repository defaults and existing PRs in the
background. Dependency edges determine publication order; duplicate destinations,
cycles, and cross-repository bases are rejected.

For each selected workspace, the host checks that the preview is still current,
fetches its PR base, and runs an ordinary conversation turn to merge when needed,
then build and test. A dependent workspace requires its parent to be published
at its current head, either already or earlier in the same plan. Preparation can
advance `W`; publishing the prepared result leaves workspace pointers unchanged.
The host rejects interruption, missing base ancestry, unresolved conflicts,
reserved `.caos` state, or a workspace changed since preparation. It records the
reviewed destination and integrated base in the workspace settings, then uses
the leased publisher and `gh` to find an open PR by repository and head branch.
An existing PR gets its base updated while retaining its title; otherwise a new
PR is created. Cancelling stops further work in the batch. A partial failure
reports completed PRs and the workspace where publication stopped.

`/publish-branch` uses the selected workspace's destination and skips preparation
and PR creation. Squash and conversation publication remain deferred.

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


## Named workspace navigation

`Ctrl+O` and `/workspace` open a workspace picker showing repository, changes,
publication state, and base. Enter selects a workspace; `n` creates from the
highlighted workspace's current commit. Tab in the creation form chooses a
separate change or a dependent change. `/workspace create <name>` uses the
selected workspace, while an explicit revision remains available as
`/workspace create <name> <rev>`. `/workspace stack <name>` records a dependency
on the selected workspace.

Selection belongs to the client. Each submitted request captures its focus in
its immutable model configuration, and the model receives a named workspace
inventory on each round. With multiple workspaces, tool calls must name their
target; changing the picker cannot redirect a call already in flight. Local
navigation and informational commands remain usable during a running turn or
publication without submitting a message.

The agent's `workspaces` tool lists, creates, removes, and promotes workspaces.
Creation shares the host's rules and publishes settings and the operation's
receipt atomically. Subagent working copies remain inside their child
conversations; promotion of a completed child creates a visible workspace when
that result deserves separate review. Ordinary harvesting still applies the
child result to an existing workspace.


### Updating workspace stacks

A workspace base records the commit it has incorporated. In the workspace picker,
“needs update” means that a parent workspace (or one of its ancestors) has moved.
Use **Update stack**, picker **u**, or `/workspace update [<name>|--all]` to update
a connected stack in dependency order. Remote branch bases are checked at that
point; the picker does not poll repositories.

Updates use the same three-way reconciliation as other workspace changes and
preserve both histories. Each workspace head and its new base pin land under one
conversation lease. An upstream rewrite is an error. A conflict stops the batch
and leaves that workspace unchanged, reporting the source commit and paths;
resolve it with the existing merge tool, then update again. Earlier successful
updates remain visible. Repeating an up-to-date operation creates no commits.

### Repository attachments and integration inputs

The workspace picker’s **a** action attaches a repository URL and branch (or
full commit), importing its code without checking it out over the client.
`/workspace attach <name> <repository> [<branch>|<commit>]` does the same.
A branch attachment remembers its base for Update stack; a commit attachment
stays pinned. Forking a workspace inherits its repository. Repository identity
comparison treats equivalent SSH and HTTPS spellings alike, while Git retains
the original transport URL for authentication.

Each workspace supplies its own AGENTS.md and repository tool schemas. In a
multi-workspace conversation, use `workspace_tool` with explicit workspace,
tool, and arguments; short names remain available for a single workspace.
Named merge/history refs are captured per repository when the request starts.
A repository attached later needs a new request to refresh those named refs;
full commit hashes remain usable.

Bash can read other workspaces through `inputs`, for example
`{"api":{"workspace":"api","paths":["schema"]}}`. The command reads
`$CAOS_INPUTS/api/schema`; these are immutable snapshots captured in the
tool’s request. Only the target workspace is staged back. This permits
integration builds without introducing live shared working directories.
