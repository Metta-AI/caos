# Conversations, code stacks, and publication

Conversations record content and execution in Git. They reference ordinary
code commits through gitlinks. Files and folders organize source trees and
review boundaries; a publication destination is optional until publishing.
The TUI browses this filesystem read-only, showing file contents or the diff
between adjacent code boundaries. The client imports repositories, checks out
code locally, and publishes explicitly named snapshots. Imports, pushes, and
PR links remain visible as CAOS messages in conversation history.

Imports are gitlinks to code commits. A local import snapshots the checkout's
disk contents: an unchanged snapshot reuses HEAD; a changed snapshot becomes
a child of HEAD. Editing an ordinary conversation file changes `C`; editing
a gitlink's contents also creates a new `W`.

| Thing | Where | Meaning |
| --- | --- | --- |
| `C` | CAOS Git | Conversation content in its tree; execution events in its commit message. |
| `W` | CAOS Git | An ordinary code commit, referenced by an entry in a conversation tree. |
| `P` | Destination Git | A publication branch pointing to an existing `W`. |

Many `C` commits can reference the same `W`. Execution bookkeeping does not
create a code change. A named PR boundary can span many `W` commits.

```mermaid
flowchart TB
    subgraph caos["CAOS Git"]
        before["C_0: user message<br/>C_1: record run request<br/>C_2: worker starts run<br/>C_3: model requests bash<br/>C_4: bash starts"]
        after["C_5: bash completes edit<br/>C_6: model replies<br/>C_7: run finishes"]
        before -->|conversation continues| after
        before -.->|feature/01-parser| W0["W_0: original code"]
        after -.->|feature/01-parser| W1["W_1: edited code"]
        W1 -->|Git parent| W0
        after -.->|feature/00-base| W0
    end
    subgraph destination["Destination Git"]
        P["P: feature/01-parser"] --> published1["W_1"]
        published1 -->|Git parent| published0["W_0"]
    end
    W1 -.->|publish same commits and ancestry| published1
```

This example dispatches bash to a worker; each listed `C` is a separate commit.
Editing the named snapshot advances its `W`; the base stays unchanged.
Publishing pushes that exact code commit.

## Conversation commits

**Trees hold canonical content; commit messages hold execution history.**

The tree contains the title, canonical transcript, ordinary conversation files,
and code references. Protocol metadata lives under `.caos/`; ordinary content
and code references live directly at paths outside it. The format marker in
`.caos/format` is `caos-conversation-v5`.

Structured commit messages record run requests, worker claims, tool calls and
results, and background/subagent activity. The TUI derives the latest relevant
state for each operation by replaying those events.

A normal `C` parents the previous `C`; a new root starts from a fixed genesis
commit. Forks retain their source history through a Git parent, which identifies
their origin. Conversation parent edges never point to code commits.

One canonical, **unsharded** transcript lives under `.caos/transcript`.
Earlier versions are available through conversation history.

### Why recording a run is a separate commit

1. Commit the user message as `C_0`.
2. Build a computation request using `C_0` as its input snapshot. Computing
   its hash creates no conversation commit.
3. Create `C_1` with an event recording that request's hash, input snapshot,
   model/settings, and queued status.

The request hash depends on `C_0`; including it inside `C_0` would make the
hashes depend on each other. Step 3 identifies the concrete run to execute.

The worker subsequently records that it has started the run. Model responses
and run completion also create `C` commits. Dispatched tools, such as bash,
record separate start and completion commits. Inline tools, such as `read`
and `edit`, record completion without a separate start commit. An event-only
commit can reuse its parent's tree.

Events are replayed in first-parent order, never by timestamp. A fork starts a
new execution context while retaining ancestral tool results needed by its
canonical transcript. Forking requires a quiescent request; it does not resume
the source's background work.

## Importing code

`/import imports/repo/base /path/to/repo` is a client operation:

1. Snapshot the checkout's disk contents into a Git tree, including uncommitted
   and untracked files. Git's ignore rules apply to untracked files; tracked
   files remain included. Preserve executable bits and symlinks; omit `.git`.
2. If that tree matches HEAD's tree, reuse the exact HEAD commit `W_0`.
   Otherwise create `W_0` with that tree and HEAD as its parent. Import the
   required objects and history into CAOS without changing the source checkout's
   index, branches, or files.
3. Create a conversation commit parented by the previous `C`, adding a gitlink
   at `imports/repo/base` that points to `W_0`, then append a system transcript
   entry recording the destination path and imported commit. Publish both commits
   with one conversation-ref update against its expected previous head. The TUI
   renders system entries as `CAOS`, so the import remains visible after reload.

Local disk imports require a Git checkout root with an existing HEAD. Linked
worktrees are supported; plain directories, individual files, and subdirectory
snapshots are not. An explicit revision, such as
`/import imports/repo/base /path/to/repo main`, imports that commit instead of
disk changes. Remote URLs import the requested revision or their default branch.
Quote paths containing spaces. Local paths are absolute or relative to the TUI's
launch directory; `~` and environment variables are not expanded.

A sibling `imports/repo/base.source.json` records a portable repository URL and
optional default branch. Local discovery reads `origin` and `origin/HEAD`;
remote discovery reads the advertised default. Local paths and credential-bearing
URLs are omitted. Metadata stays outside the imported code. Importing requires
no publishing destination and establishes no local checkout destination.

## The agent's filesystem

The harness supplies source-tree organization and publication conventions to
every model call, including subagents. Agents preserve bases, organize review
boundaries, and integrate delegated changes using ordinary file operations;
users specify the desired work and review structure. Imports conventionally live
at `imports/<repo>/base`. A sibling `<import-path>.source.json` records portable
repository details when available; imports can come from different repositories.
The agent preserves imports. To build a PR stack, it copies an
imported gitlink to `<feature>/00-base` and `<feature>/01-parser` before editing.
Publication commands name the gitlink and base branch explicitly. The repository
URL comes from the command or unambiguous import provenance; the preview shows
the resolved destination before confirmation.

Commands start at the conversation root. Memories, skills, notes, and source
trees share one path space. A commit-valued entry appears as a directory whose
contents are that commit's tree; a regular file containing a SHA remains a file.
Nested commit entries follow the same rule.

The file tools and grep use conversation-relative paths, such as
`memories/project.md` or `feature/01-parser/README.md`. Grep traverses source trees.
Bash runs from this root, with an optional relative `cwd` for a single call.
Its `paths` list is always relative to the conversation root: declared
directories include their descendants; undeclared contents remain lazy.

Use ordinary filesystem operations to organize content. This example starts
with a gitlink imported at `imports/caos/base`:

```sh
mkdir -p feature
cp -a imports/caos/base feature/00-base
cp -a imports/caos/base feature/01-parser
# After completing the first change, start the next:
cp -a feature/01-parser feature/02-errors
```

The initial two copies reference the same `W_0`. Editing
`feature/01-parser/README.md` produces `W_1` with parent `W_0`; the next
conversation commit advances only `feature/01-parser`. The import and
`feature/00-base` remain unchanged. Several shell commands in one tool call
are saved together, not as a separate conversation commit per shell command.

A writable projection records each source directory's original commit in an
extended attribute. `mv` and `cp -a` preserve it. On storage, an unchanged
directory retains the exact commit, including signatures; edited content
becomes a child commit. Ordinary directories remain Git trees. Copying without
preserving extended attributes copies files but loses the commit boundary.
This metadata is local to the projection, not another conversation registry.

The harness prepares editable files before each tool call; agents do not run a
checkout command. Copy an existing source directory with `cp -a`. To add a
reference to a commit already in CAOS, use the ordinary storage primitives:

```sh
caos get-hash <commit> /cas/source
ln -s /cas/source feature/01-change
```

Storing the tool result records that link as a gitlink. The next tool call
exposes it as an editable directory. Local or remote repository imports happen
through the client; host filesystem paths are not implicitly available to workers.

Call a repository tool with `run_tool` and a conversation-relative path, such
as `feature/01-parser/caos-tools/test`, plus its arguments. The harness resolves
the tool against the captured snapshot and dispatches its content-addressed
request. Its input is the outermost source tree on that path: the first
commit-valued entry reached from the conversation root, even if the tool lies
inside another gitlink. A tool outside source trees receives the conversation
tree. Repository tool schemas and instructions are shown with their owning paths.

A shell result may change conversation files and several source trees together.
Apply it atomically against the captured input: retain concurrent unrelated
edits, reconcile concurrently edited source commits, and retain a conflicting
proposal without installing half the operation. Agent tools cannot change
conversation protocol metadata under `.caos/`.

## Code references and stack directories

Code references are Git commit-valued tree entries (gitlinks, mode `160000`).
A reference's value is a `W`; its identity does not depend on the repository
from which it was fetched.

References may occur anywhere outside `.caos`. The TUI walks ordinary
directories and lists each commit-valued entry it reaches, stopping at that
entry. Nested gitlinks remain traversable by file tools but are not separate
TUI source-tree targets.

A single reference is enough for simple work. For reviewable changes, copy an
imported gitlink into a feature directory and follow this convention:

```text
paintbot-feature/
  00-base          -> W_0   # preserved starting commit
  01-add-targeting -> W_1   # first PR boundary
  02-improve-it    -> W_2   # second PR boundary, currently being edited
```

Each sibling gitlink names a snapshot. In ascending filename order, the first
is the base; every later entry is a review boundary. Number prefixes make this
order clear but have no special syntax. Names such as `dirty` have no special
meaning. Work directly in a named boundary, then copy it to the next name when
starting another reviewable change. Earlier snapshots remain unchanged.

The browser lists entries in descending filename order. It compares a selected
gitlink with the next gitlink below it; a folder previews its newest two
gitlinks. The oldest entry has no comparison and shows content. Publication
commands name their base branches explicitly. Naming a boundary does not
squash intermediate Git commits.

Work, delegation, merging, tests, and review boundaries need only the recorded
commits. No publication destination is required to prepare a stack. Fetching a
newer remote tip does not integrate it; the agent must merge it explicitly.

Creating, copying, renaming, or removing references is ordinary tree editing.

### Editing, updating, and delegating

Tools receive immutable input snapshots. Capture these when work starts so
changing UI selection cannot redirect an in-flight operation. Git operations
such as merge name the source tree they operate on; filesystem tools share
the conversation root.

To update a stack, the agent gives the user a concrete `/import` command for
the remote commit, then merges the imported commit into the chosen entries
and moves or copies directories as needed.
There is no stack-update operation or background refresh of remote branches.
Directory ordering does not replace integrating Git histories.

Subagents start from one content tree with fresh protocol metadata. By default
they receive all conversation content; optional `paths` selects files,
directories, or gitlinks to copy at the same paths. Select a gitlink as a whole
source tree; use ordinary file edits for finer changes inside it.

`harvest_agent` applies the child's changes since its initial snapshot, optionally
restricted by `paths`. It uses the same atomic application as file tools:
ordinary files and multiple source trees can arrive together, unrelated parent
edits survive, and conflicts retain the proposal without partial application.
Child identity, initial conversation head, run request, and terminal head are
recorded in events; content is read from those commits.

### Starting a client

The client starts from a CAOS harness independently of target code. A
conversation begins from an optional content tree; without one it starts empty.

The invocation names the workers with typed image arguments, for example
`caos tui --llm-step:@=std/llm-step --llm-call:@=std/llm-call`.
Paths resolve in the harness, not an attached source tree. Hash and Git-locator
image arguments also work; no root DEPS entry is required.

Add `--import imports/caos/base` to snapshot the launching checkout
at that exact path as a gitlink. Add `--base HEAD` (or another revision) to
exclude disk changes and import that commit. Cloud sessions likewise start
from a stable CAOS client repository/environment and
attach target code afterward. This also makes bootstrap caching independent
of the target repositories.

Startup checks local credential configuration without resolving worker images.
The sidebar reads conversation metadata; opening or modifying a conversation
validates its history. Reader grants are resolved when a request is prepared.

Credentials, server choice, and client caches remain local. Checkout destinations
are local preferences keyed by server, conversation, and gitlink path. Neither a
code commit nor a publishing URL identifies a directory on the user's machine.
These preferences survive harness updates; renaming a gitlink requires choosing
its checkout destination again.

## Publication

`/pr <conversation/gitlink> <base-remote-branch> [remote-URL]` prepares one PR.
The gitlink supplies the exact code commit and its path supplies the branch
name. The base branch is always explicit; directory order never chooses it.
For example, publish a stack one boundary at a time:

```text
/pr paintbot-feature/01-add-targeting main
/pr paintbot-feature/02-improve-it paintbot-feature/01-add-targeting
```

The optional remote is a repository URL, not a local remote name such as
`origin`. Without it, the client uses import provenance matching the oldest
sibling's commit. Multiple matching imports must agree on the URL; differing
default branches do not matter because the command specifies the base. Missing
or ambiguous provenance requires an explicit URL. No remembered publication
destination or `.base-url` policy is consulted.

The command fetches the destination and opens a read-only preview of the source
path, commit, repository, branch, and base. Enter confirms; Escape cancels.
Changing the request requires a new command and preview. The client uses its
Git and GitHub credentials to push that exact commit and open or reuse a PR.
No local working checkout is required. `/publish-branch <conversation/gitlink>
[remote-URL]` uses the same flow without creating a PR or requiring a base.

Reject changed content, remote drift, conflict markers, and unrelated histories.
Check source-tree metadata before preview and again before pushing: a nonempty
`.caos/conflicts` ledger requires resolution. Recording an edited source tree
removes an empty ledger and prunes its `.caos` directory if empty, for both bash
and inline edits. Unchanged commits, unresolved entries, other metadata, and
conversation protocol files are preserved. Publication rejects any remaining
`.caos` entry; it never strips files or rewrites the selected commit.
The source and PR base must share an ancestor. If the source does not contain
the fetched base tip, the preview instead offers to import that exact commit and
ask the agent to integrate it. Enter confirms both actions: import under
`imports/pr-base-<commit>/base`, then send a message naming the base and source
paths and requesting a merge or rebase plus tests. The import and request stay
in the original conversation and preserve the user's draft. A failed import
sends no message; retrying reuses an identical import. Nothing is published by
this action. Run `/pr` again to review the updated source and current remote base.
Before integration, the agent checks whether the proposed PR includes inherited
changes beyond the requested work. Merging upstream retains those changes;
transplanting only the requested edit onto another base needs a separate decision.
Publication itself never runs an agent or edits code. A base branch
for a later PR must already exist remotely, so publish earlier PRs first.

Find existing PRs by repository and branch. Inspect destination refs after an
interrupted push before retrying. A confirmed push appends a `CAOS` transcript
entry with the source commit and destination. Successful PR creation or update
appends another entry with the PR URL, branch, and base. These remain visible after reopening the conversation;
a failed PR operation never records a successful PR.
A renamed gitlink changes the proposed branch, which appears in the preview.

## Client interactions

- `Ctrl+O`: browse conversation files and source-tree diffs. Highlighting an
  entry previews it; arrows navigate folders and gitlinks. The whole browser
  is read-only, including `.caos`.
- `/import <path> <source> [revision]`: import a checkout snapshot or Git revision
  at an unused conversation path, as described above. Existing differing
  provenance is rejected, not overwritten.
- `/checkout <conversation/gitlink> [directory]`: check out the named commit
  with detached HEAD in an existing clean Git checkout or an empty/new directory.
  Remember the destination locally; omitting it reuses that gitlink's destination.
  Relative paths resolve from the launching directory. A successful checkout
  also selects the source for local edit submission.
- `/update-tree <message>`: submit local edits to the selected code snapshot
  with a user message.
- `/pr <conversation/gitlink> <base-remote-branch> [remote-URL]`: fetch and preview
  one PR. Enter confirms publication or the offered base-import and integration
  request; Escape cancels.
- `/publish-branch <conversation/gitlink> [remote-URL]`: preview and confirm a
  branch push without creating a PR.

Checkout and publication use explicit paths, independent of browser selection.
There are no Ctrl+L or Ctrl+P bindings.

The browser pins the conversation head when opened; refresh loads the latest
head. Ordinary files show contents. Inside a source boundary, files show their
diff against the preceding boundary, including deleted files. The preview labels
the compared boundaries and hashes. There are no shell commands or Apply controls.

The client handles host-side imports, checkout, and publication. The harness
instructs the agent to give exact TUI commands or keys, with the intended paths,
when user action is needed. A prose request to the agent does not run a client
command. After an ordinary `/import`, a new user message resumes the agent.
The PR-base handoff sends its integration request as part of confirmation.
The agent organizes conversation content using files and folders. There is no
active source tree in execution: file tools use conversation-relative paths, and Git
operations explicitly name their target. UI selection cannot retarget a run.

## Scope

Separate stack directories can attach separate repositories. Consuming
unpublished code across them requires explicit dependency configuration.
Git-by-hash endpoints, automatic DEPS projection, conversation merging, and
transcript compaction commands are not currently implemented.
