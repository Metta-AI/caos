# Chat v3: conversation and code as separate histories

| What | Where | Referenced by |
| --- | --- | --- |
| `C` conversation commit | `caos` remote | `refs/caos/v3/conversations/<hex(id)>/head` |
| `W` workspace commit | `caos` remote | sha in a conversation commit's tree |
| `P` publication branch | destination repository | `refs/heads/<branch>` points to a workspace commit |

## Conversation commits

`C` commits contain:

- `tree`: complete conversation state, below.
- `parent`: exactly one. `G3` for a new root, otherwise the previous `C`
  (or the source `C` when forking another conversation).
- `message`: transition kind, such as `message.append` or `tool.complete`.
- `author` and `committer`.

Conversation commits never have workspace commits as parents. They reference
workspace commit hashes through files in their trees.

`C`'s tree contains:

```
# Identity and metadata
.caos/format                                        protocol version: "3"
.caos/identity.json                                 ID, root/fork origin, and subagent parent/spawning call
.caos/title                                         displayed title

# Transcript
.caos/transcript/<shard>/<ordinal>-<message-id>.json  speaker, model, content blocks
.caos/transcript/<shard>/<ordinal>-<message-id>/      message payload files

# Workspaces
.caos/workspaces/<name>/commit                       current code commit hash
.caos/workspaces/<name>/initial                      starting commit; baseline for changes and rollback limits
.caos/workspaces/<name>/origin                       optional source label and original tree hash
.caos/workspaces/<name>/config.json                  repository, publication branch, upstream and integrated commit

# Conversation-owned files
files/                                              files separate from workspace code

# Agent turns and tools
.caos/requests/<id>.json                             starting snapshot, model settings, calls, status, outcome
.caos/requests/active                                active request hash; absent when none
.caos/tools/<request>/<round>/<call-id>.json          input workspace, status, result, applied changes
.caos/tools/<request>/<round>/<call-id>/              tool arguments and output files

# Background work
.caos/async/                                         task status and results
.caos/subagents/                                     child conversations, spawn inputs, results, applied changes

# Publication
.caos/publications/                                  destinations, planned commits, expected remote tips, outcomes
```

### Transitions and turns

Each transition creates a new `C`. Its kind determines which changes are
allowed; validation rejects unrelated changes and no-op transitions.

A turn starts by appending the user message and admitting a request. The worker
claims it, calls the model, records its response, and executes its tools. This
repeats until the turn finishes or fails. Interruptions are handled at worker
boundaries.

Dispatched tools record `tool.start` before execution and `tool.complete`
afterward. Immediate tools can complete without a start record. Results record
both what the tool returned and how its changes were applied.

### Background work and subagents

`run_async` returns a task handle while computation continues. A subagent is a
separate conversation rooted at `G3`; its identity names the parent conversation,
parent commit, and spawning call. It receives the selected workspace, if any,
but does not inherit the parent's transcript or files.

Completion is recorded in the parent. Harvesting applies child changes to an
existing workspace; promotion creates a separate workspace for review.

TODO: Unify bookkeeping around:

- **Turns:** model loop, interruption, outstanding calls. Rename conversation
  `requests` to distinguish them from CAOS computation requests.
- **Calls:** arguments, responses, applied changes, and an optional task reference.
- **Tasks:** shared status, results, cancellation, and recovery. Start with
  `async` and `subagents`, treating child conversations as a task variant;
  retain spawn information and application records.

Launching, finishing, and applying work are separate events. Calls identify
occurrences; computation hashes identify content, so calls can share cached
work but still need separate responses. Immediate tools need no extra task
record. Share lifecycle code with dispatched tools where useful, using explicit
variants rather than one record full of optional fields.

### Forks, titles, and archiving

A fork starts a new identity from an existing `C`. The source must have no
active turn, tool execution, async task, or publication. Running child records
are dropped from the inherited state. Renaming changes `.caos/title`.

Archiving moves a conversation out of the active list without deleting its
history.

### Validation

JSON records use canonical bytes so hashing is stable. Readers check the commit
and reconstruct its declared transition; the resulting tree must match. See
[record formats](../rust/crates/conversation-protocol/src/v3/records.rs) and
[validation](../rust/crates/conversation-protocol/src/v3/validate.rs) for exact rules.

## Workspace commits

A conversation has zero or more named workspaces. Each points to a `W`: a Git
commit containing a code tree, parents, message, author, and committer. Edits
create descendant commits; merges can have two parents. The pointer in `C`
tracks the current head without requiring a separate named workspace branch.

### Sources and upstreams

Attaching code imports existing commits and ancestry without rewriting them or
changing the local checkout. A branch attachment remembers its upstream; an
attachment by commit stays pinned. Workspace inputs cannot contain reserved
`.caos` entries other than `.caos/conflicts`.

Currently, `config.json` holds the repository, publication branch, and upstream.
The upstream is a repository branch or another workspace, plus the exact commit
already integrated. Workspace dependencies must stay within a repository and
cannot form cycles. A workspace with dependents cannot be removed.

TODO: Separate these concepts:

- **Source:** optional pinned locator using the existing `:@@=` syntax. Reuse
  its parser, retaining the commit and ancestry rather than only a tree. Check
  whether `origin` has callers beyond fixtures before replacing it.
- **Upstream:** branch or workspace to integrate from, plus the incorporated
  commit. An immutable source locator cannot replace a moving upstream.
- **Publication destination:** derive a default; remember repository and branch
  when chosen or published. Keep this with publication settings. Stacked PRs
  still need their parent's destination.

### Selecting and creating workspaces

`Ctrl+O` or `/workspace` opens the picker. Create a workspace from the selected
snapshot or an explicit revision; stack it on another workspace when it depends
on that work. `/workspace attach <name> <repository> [<branch>|<commit>]` attaches
another repository.

Selection is local UI state. A submitted turn captures its focus; with multiple
workspaces, tool calls name their target. Switching selection cannot redirect
an in-flight call. Navigation and informational commands do not send messages.

Each workspace supplies its own AGENTS.md and repository tools. Cross-workspace
bash inputs are immutable snapshots; only the target workspace's output is
adopted. Named repository refs are captured at turn start.

### Applying changes and updating stacks

Tools, subagents, and `/update-tree` use the same reconciliation rules. A
proposal must descend from its declared base. Already-applied proposals do
nothing; compatible descendants advance directly; otherwise try a three-way
merge. A conflict records the candidate and leaves the workspace pointer alone.
Equal trees are not enough to discard a commit: its ancestry may matter.

`/update-tree` includes local committed and uncommitted changes, using the merge
base with the workspace. Unrelated histories or multiple merge bases are errors.

`/workspace update [<name>|--all]` integrates upstream changes in dependency
order. Each workspace head and incorporated upstream commit update together.
An upstream rewrite or conflict stops the batch; earlier successful updates
remain. Resolve conflicts with the merge tool, then retry.

A hash written in `C` does not keep `W` reachable to Git's garbage collector.
CAOS currently disables automatic Git GC; retention and erasure policy remain
undesigned.

## Publication branches

Publishing points a destination branch at a workspace's existing commit.
The current `preserve` policy keeps its ancestry and creates no extra code
commits. It records outcomes in `C`, leaving workspace pointers unchanged.

Destinations can be configured. Default names are `caos/<id>` for a single
workspace and `caos-workspaces/<id>/<workspace>` for multiple workspaces;
existing publication destinations are retained.

### Preparing PRs

`Ctrl+P` previews workspaces, destination branches, and PR bases. Select which
to publish; parents publish before dependents. Each selected workspace gets a
preparation turn to merge its PR base if needed, then build and test.
Preparation can advance `W`.

Publication requires the base's ancestry, no conflicts or `.caos` entries, and
an unchanged prepared workspace. A dependent's parent must be published at its
current head. The host pushes and uses `gh` to open or reuse the PR. Cancellation
stops further work; completed publications remain.

`/publish-branch` skips preparation and PR creation. Squashing and publishing
conversation content are not implemented.

### Pushes and recovery

Publishing must preserve the destination branch's existing history. If someone
updates that branch while the push is being prepared, the push is refused.
If a push is interrupted, the host checks where the branch actually points
before retrying. The publication record says whether the push completed,
conflicted, or remains uncertain.

## Host and launcher inputs

The TUI starts `main` from the launching checkout's HEAD or `--base`/`--from`.
Without a checkout, or with `--empty`, it starts with no workspace. Line clients
use their caller's checkout and HEAD/`--base` defaults.

The TUI keeps its bundled harness in a separate client store. Attaching or
editing code never overwrites it. Checkout commands act on the original matching
checkout. Server and secret-store choices belong to the local client;
credentials are never copied into conversation or workspace trees.
