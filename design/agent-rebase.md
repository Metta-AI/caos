# Agent stacks and replay

## Starting a feature

Import commit A into `feature/00-work`, then write A's full commit id to `feature/00.base`:

```text
feature/
  00-work       # gitlink to A
  00.base       # text file containing A's commit id
```

Normal tool calls edit `00-work` and advance its gitlink. After many calls, its history might be `A — w1 — w2 — … — W`.

When the work is ready to become a named layer, `git-add-layer` makes a commit C with W's tree, parent A, and an intentional message. It replaces `00-work` with the named layer and starts the next
work at C:

```text
feature/
  00-first-step # C
  00.base       # A
  01-work       # C
  01.base       # C
```

The named layer has one commit above A. Subsequent work builds on C, so the tool-call history doesn't enter later layers either. The original work remains available in conversation history.

This direct squash requires the work's recorded base to be the intended preceding layer. If that layer has changed, replay the work onto it first; changing the parent alone wouldn't incorporate its
changes.

## Stack representation

- numeric prefixes order the layers, starting at 00 and increasing consecutively
- each number has one `<number>-<name>` gitlink and one `<number>.base` text file containing a full commit id
- `00.base` records the bottom layer's base; each later `.base` records the predecessor commit that layer was built on
- a later `.base` may differ from the preceding layer's current tip, but the stack needs restacking before publication
- other files in `feature/` are allowed and are preserved
- a work layer can contain many tool-call commits; promoting it squashes those commits
- replay can deliberately produce several commits within a named layer

## Tools

These are repository tools in `std`, each with its own `.caos-expr` and help. `run_tool` and `tool_help` evaluate along their paths.

Tools that change the conversation declare themselves writers. A writer receives the conversation tree as `in` and returns a `proposal` tree alongside its report. Unchanged parts can be referenced by
hash; returning a proposed tree doesn't require loading all its files.

`llm-step` records the input snapshot and atomically applies the proposal with the tool result. For these stack writers, it checks that the stack directory still matches the invocation's input.
Changes elsewhere are preserved. A concurrent change to the stack makes the proposal stale; the handler must not automatically merge rewritten gitlinks.

The Git operations use objects in the server's store. Workers don't need to check out a repository or materialize its source tree.

### `git-merge-tree`

- inputs
  - `merge-base`: base tree
  - `ours`: our tree
  - `theirs`: their tree
- outputs
  - merged tree
  - Git's conflict report

### `git-commit-tree`

- inputs
  - `tree`: tree
  - `parents`: parent commit ids
  - `author`: author identity and timestamp
  - `committer`: committer identity and timestamp
  - `message`: commit message
- outputs
  - new commit id

### `git-add-layer` — writer

- inputs
  - `stack`: stack directory
  - `name`: name for its final work layer, without the number prefix
  - `author`: author identity and timestamp
  - `committer`: committer identity and timestamp
  - `message`: commit message
- outputs
  - new layer name and commit id
  - proposal replacing the final `<number>-work` with the named, squashed layer and creating the next work gitlink and `.base`, both pointing to the new commit

The tool requires the final layer to be named `<number>-work`, checks its recorded base against its predecessor, and uses that base as the new commit's parent. For layer 00, the parent is `00.base`.
It won't run during an active replay.

### `git-rebase-i` — writer

- inputs
  - `stack`: stack directory
  - `plan`: `<stack>/rebase/plan` when starting
  - `action`: `continue` when resuming
- outputs
  - on conflict: stopped instruction, draft path, conflict report path, and a proposal recording progress under `feature/rebase/`
  - on completion: output layer names and commit ids, executed plan, and a proposal replacing the numbered stack entries and removing `feature/rebase/`

## The plan

The format borrows from Git's interactive rebase todo, but isn't identical. Each instruction uses `command=value`. Commit references are full ids; examples use letters for readability.

- `onto=H`: start at existing commit H, keeping its history unchanged; must appear once, first
- `pick=B`: apply the change from B's original parent to B as one commit on the current output tip
- `pick=A..B`: apply the net change from A's tree to B's tree as one commit, without replaying the intermediate commits
- `squash=B` or `squash=A..B`: apply the same change as the corresponding pick, but fold it into the preceding output commit, keeping that commit's parent and author and concatenating its message with
  B's, separated by a blank line
- `amend=<line>`: replace the latest output commit's message with the contiguous block of `amend=` lines, joined with newlines; an empty `amend=` supplies a blank line
- `drop=B`: explicitly omit B; equivalent to leaving its pick out, without undoing changes included by another pick or range
- `branch=00-name`: record the current tip as a numbered output layer

`pick` uses B's author and message. `amend` changes only the message. The replay records a fixed committer, keeps empty commits, and removes invalidated signatures. An unchanged single-commit pick can
reuse the original commit.

Single-commit picks and squashes require one parent. Ranges describe a linear ancestor-to-descendant segment; A may equal B. Gaps in the selected changes are intentional omissions.

Messages are inline, not separate files. Everything after the first `=` on an `amend` line is literal message text, including spaces, `#` and further `=` characters. A contiguous message block is
processed as one instruction. Blank lines and whole-line comments outside message blocks are ignored.

Each `branch` writes a gitlink and its numbered `.base` under `feature/rebase/stack/`. The first base is `onto`; later bases are the preceding output layer tips. Output names and the number of layers
can differ from the original stack; numbers start at 00 and remain consecutive.

`branch` seals that layer. `squash` and `amend` require an output commit created since the last branch boundary, so they cannot rewrite the starting base or an already recorded layer. Multiple picks
before a branch produce multiple commits in that layer. An empty layer may record the same tip as its predecessor. The final tip must be recorded by a branch.

To change or split a commit before replay, make replacement commits with `git-commit-tree` and reference them in the plan.

For a current output tip H, `pick=A..B` means:

```text
tree = git-merge-tree(merge-base=tree(A), ours=tree(H), theirs=tree(B))
commit = git-commit-tree(tree=tree, parents=[H], ...)
```

`pick=B` uses B's original parent in place of A. `squash` does the same merge against H's tree, but the resulting commit keeps H's parent instead of having H as its parent.

## Running and resolving conflicts

The agent writes or copies the plan to `feature/rebase/plan` and calls `git-rebase-i(stack="feature", plan="feature/rebase/plan")`. There is one active replay per stack; other stacks can replay
independently.

The tool validates the plan and executes until completion or a conflict. It proposes this state when paused:

```text
feature/rebase/
  plan          # instructions, fixed metadata, and progress
  stack/        # output layers recorded by branch, if any
  work          # draft gitlink containing the conflicted tree
  conflicts     # Git's conflict report
```

The original numbered entries stay in place until completion. The plan records their tree id and the replay's committer. Tool-written `done` records identify completed instructions and resulting tips;
a `here` record identifies the paused instruction and reason. These records track execution, rather than adding user commands or a separate state file.

The latest completed result is the current output tip, even if no branch has recorded it yet. A conflicting pick or squash remains pending.

The agent edits the draft with normal tools. Text conflicts have markers; `conflicts` contains Git's report, including object ids and modes for cases such as modify/delete or binary conflicts. The
report stays outside the source tree.

After resolving and checking the draft, the agent calls `git-rebase-i(stack="feature", action="continue")`. This explicitly accepts the draft as resolved. The tool uses its tree to finish the pending
pick or squash, clears the draft and report, and proceeds. The tool-call commits used to edit the draft don't enter the rebuilt history. No worker stays running during the pause.

While paused, the agent may change unfinished instructions, including pending message blocks. Replacing the paused instruction discards its draft and restarts from the last completed tip; changing
only later instructions preserves the draft. The tool validates the edited plan before proceeding. The original snapshot, `onto`, committer, completed instructions and records, pause record, and
recorded output layers stay fixed.

## Aborting and completing

To abort, delete `feature/rebase/`. The original stack is still there. Applying a writer result must reject a stale invocation, including one from before an abort and recreation of the replay
directory.

Before completion, the tool compares the original numbered-entry tree id with the current numbered entries. Added, removed, or changed layers and bases make the comparison fail; ordinary files are
excluded. This check reads the entries, not their source contents.

On a mismatch, the tool reports the problem and leaves the stack and replay intact. Otherwise its proposal replaces the numbered entries with `rebase/stack/`, preserves other files, and removes
`rebase/`. The check and application are atomic.

The result includes the executed plan and output commit ids. Earlier plans and drafts remain in conversation history.

## Example

Suppose the source history is `P — A — B — C — D`. We want to combine A and B, omit C, and put D in a second layer, all above a newer base H:

```text
onto=H
pick=A
squash=B
amend=Add authentication
amend=
amend=Support session cookies and API tokens
branch=00-auth
drop=C
pick=D
branch=01-logging
```

The first pick replays P to A on H. The squash folds A to B into that result, and the amend block sets its message. The resulting commit S has parent H; the first branch records S.

The second pick applies only C to D on S, producing T with parent S. It may conflict if D depends on the omitted change. After resolution and completion:

```text
feature/
  00-auth       # S
  00.base       # H
  01-logging    # T
  01.base       # S
```

To combine A and B in one merge instead, replace `pick=A` and `squash=B` with `pick=P..B`. This applies their net change in one step; replaying them separately can encounter intermediate conflicts
that the range doesn't.

If writing plans takes too many steps, we can add a tool to prepare one later.
