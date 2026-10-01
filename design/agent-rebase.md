# Agent stacks and replay

## Stack representation

A feature is a directory containing numbered gitlinks and their bases:

```text
feature/
  00-auth       # gitlink to A
  00.base       # text file containing P's full commit id
  01-tests      # gitlink to B
  01.base       # text file containing A's full commit id
  notes.md
```

Each `.base` records the commit the layer was built on. If A is rewritten to A', B's `.base` still records A: ancestry alone cannot recover that boundary after a rewrite. The stack needs replaying
before publication when a recorded base differs from its predecessor's current tip.

Numbers start at 00 and are consecutive. Names such as `work` have no special behavior. A layer may contain several commits. Other files are allowed and are preserved; an output layer cannot overwrite them.

Publication uses the [branch push API](agent-publish.md), which currently supports branch creation and fast-forwards. A replay that rewrites an already published tip needs a new destination branch.

CAOS disables automatic Git GC. Neither a text `.base` nor a Git tree's gitlink is a native Git GC root; a future CAOS collector must retain the commits they name.

## Tools

These are repository tools in `std`. `tool_help` and `run_tool` evaluate along their paths. Git operations use the server's stored objects; workers do not check out the source tree.

### `git-merge-tree`

- inputs
  - `merge-base`: base tree id
  - `ours`: current tree id
  - `theirs`: incoming tree id
- outputs
  - `tree`: merged tree id
  - `conflicts`: Git's native conflict report, empty when clean

The server runs `git merge-tree --write-tree` with the explicit base. The ordinary `merge` tool uses the same endpoint with two commits and lets Git find their merge base.

### `git-commit-tree`

- inputs
  - `tree`: tree id
  - `parents`: space-separated parent commit ids
  - `author`: author identity, timestamp and timezone
  - `committer`: committer identity, timestamp and timezone
  - `message`: literal commit message
- output
  - new commit id

This tool formats a raw commit and stores it with `caos put-commit`. It is useful when making a replacement commit with particular parents or metadata. Replay does not require calling it separately.

### `git-rebase-i`

- input
  - `in`: the feature directory, containing `rebase/plan` and any message files
- outputs
  - `prop`: replacement feature tree
  - `out`: resulting layer ids, or the conflicting instruction and draft location
  - `failed`: present for an invalid plan, with the input tree returned unchanged

The tool declares `@writer` and `@in`. Call it through `run_tool(path="caos-std/git-rebase-i", scope="feature")`. The definition can live elsewhere; `scope` selects the conversation directory to edit.
The worker receives only that subtree, with no conversation head or wall-clock timestamp in its arguments.

The harness records the input snapshot and applies the returned subtree through the ordinary proposal path. A concurrent change inside the scope rejects the entire replacement; edits elsewhere are
preserved. Rewritten source gitlinks are replaced together, not merged individually.

## The plan

The format resembles a Git rebase todo, but is not identical. Values naming commits are full ids; the examples use letters for readability. Blank lines and whole-line `#` comments are ignored.

The first two lines supply the starting commit and the committer for newly created commits:

```text
onto=H
committer=Name <email> 1700000000 +0000
```

Then:

- `pick=B`: apply the change from B's sole parent to B as one commit on the current output tip
- `pick=A..B`: apply the net change between A's and B's trees as one commit, without replaying intermediate commits
- `message=rebase/messages/auth`: replace the latest output commit's message with this file's bytes
- `branch=auth`: record the current tip as the next layer, automatically named `00-auth`, `01-auth`, etc

A pick takes its author and initial message from B. Ranges select endpoint trees; they do not require a walk through B's history. Omitting a change leaves it out, unless another selected range
includes it.

An aligned single-commit pick whose sole parent is already the output tip reuses B exactly. Otherwise the tool creates a commit with the plan's committer, preserving the author and removing
invalidated signatures. A range always creates a commit. A message instruction reads its path relative to the feature directory when executed; unchanged message bytes retain the existing commit id.

`message` requires a pick since the last `branch`, so it cannot rewrite the starting base or an already recorded layer. Multiple picks before a branch make a layer with multiple commits. Consecutive
branches may record the same tip. The final tip must be recorded by a branch. Layer names and count need not match the original stack.

For output tip H, a range pick computes:

```text
tree = merge-tree(base=tree(A), ours=tree(H), theirs=tree(B))
commit = commit-tree(tree=tree, parents=[H], ...)
```

## Running and resolving conflicts

Write `feature/rebase/plan` and call the tool. Every call runs the plan from the beginning. Fixed commit metadata makes unchanged instructions produce the same ids; the plan contains no progress
records. Output layers accumulate in memory until completion.

On conflict, the original numbered entries stay in place and the proposal adds:

```text
feature/rebase/
  plan          # unchanged instructions
  work          # draft gitlink, parent = output tip H
  conflicts     # Git's native report
```

The report includes object ids and modes for structural conflicts as well as text markers. Edit and test `work` using ordinary tools. Once its resolved commit is R, replace the failed pick with
`pick=H..R` and run the tool again. The same prefix reaches H; the replacement applies the resolved tree. Its author and default message come from R, so use `message=<path>` for the intended message,
or `git-commit-tree` if its author also needs changing.

Only the net change enters the output; draft-edit commits do not become ancestors of the published layer. No worker remains running between calls.

To abort, delete `feature/rebase/`. On completion, the proposal replaces the numbered entries, preserves other feature files, and removes `rebase/`. Earlier plans and drafts remain in conversation
history. Each invocation is guarded against concurrent edits; there is no separate snapshot check spanning multiple invocations.

## Example: start a feature and prepare its first layer

Import A, copy its gitlink to `feature/00-work`, and write A's id into `feature/00.base`. Ordinary edits may produce thousands of tool-call commits, ending at W. To turn that work into one intentional
commit and start another layer, write:

```text
onto=A
committer=Name <email> 1700000000 +0000
pick=A..W
message=rebase/messages/auth
branch=auth
branch=work
```

After running the plan:

```text
feature/
  00-auth       # C: W's tree, parent A, chosen message
  00.base       # A
  01-work       # C
  01.base       # C
```

Further edits start from C, so W's tool-call history stays out of later layers too. There is no separate promotion tool or special working-layer state.

## Example: restack and omit a change

Given `P — A — B — C — D`, combine A and B, omit C, and replay D above a new base H:

```text
onto=H
committer=Name <email> 1700000000 +0000
pick=P..B
message=rebase/messages/auth
branch=auth
pick=D
branch=logging
```

The first pick applies P to B in one merge. The second applies only C to D; it may conflict if D depends on C's omitted change. The result is two layers with bases H and the rebuilt authentication
tip.
