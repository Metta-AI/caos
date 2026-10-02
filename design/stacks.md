# Stacks

A stack is a sequence of PRs, each based on the one below. In caos it lives in
one conversation as **sibling gitlinks, one per layer**; Git ancestry between
them is the whole dependency. There are no local branches and no stack
metadata.

```text
imports/repo/main   -> H            the base, as imported
feature/00-base     -> H
feature/01-core     -> A   parent H
feature/02-tests    -> B   parent A
```

## Layers

- **A layer is a review boundary, not an editing step.** Each gitlink names a
  commit that may sit on many edits and merges; the name says where a PR's
  diff ends.
- **Order is by filename.** By convention the first sibling is the starting
  base (`00-base`) and each later one a boundary; number prefixes make the
  order plain. Folder order merges nothing and chooses no PR base — ancestry
  does.
- **Build a layer by copying the one below** and editing the copy:

  ```sh
  cp -a imports/repo/main feature/01-core
  cp -a feature/01-core feature/02-tests
  ```

  `cp -a` keeps the directory's commit (an extended attribute), so the copy
  is the same gitlink until edited, and its first edit is a child of the
  layer below. A copy that drops the attribute is plain files and loses the
  boundary.

## Keeping a stack current

**Layers are only ever merged into, never rewritten.** Both kinds of change
flow upward the same way:

- **A lower layer changed:** merge it into the layer above, then that into
  the next, and test the top.
- **The base moved** — what "rebase" means here: import the new tip at a
  fresh path (an import never advances an existing one), merge it into the
  bottom layer, then merge up as above.

Merging is `std/merge` with the other side's full commit hash. A conflict
still advances the layer, with markers in the files and a `.caos/conflicts`
ledger listing every unresolved path; resolve each and delete its rows
(chat.md, "Resolving source-tree conflicts").

So a layer's history is a **merge history**: every upstream change it
absorbed is a parent edge, and nothing a reviewer or a subagent saw is ever
replaced.

## Delegating layers

Two independent changes intended as a stack can be built by subagents:

1. Give two children bounded tasks against the same starting source.
2. Harvest the first child's change into `feature/01-core` and check it.
3. `cp -a feature/01-core feature/02-tests`.
4. Merge the second child's source commit into `feature/02-tests` and check
   the combined result.

Harvest applies changes at their existing paths; it chooses no boundaries.
The parent owns the stack's shape.

## Publishing

**Bottom to top, each layer to its own branch.** The first PR targets the base
branch; each later one targets the branch below it, which must already exist
remotely.

- **Agent:** `publish_source` per layer (design/agent-publish.md). A
  fast-forward only, leased on the remote head observed when the call
  starts, so a change someone else pushed is never overwritten.
- **User:** `/pr <layer> <base-branch>` in the TUI, in the same order
  (chat.md, "Publishing with `/pr`"). When the source does not contain the
  base's tip, the preview offers to import it and ask the agent to merge it
  up instead of publishing.

  ```text
  /pr feature/01-core main
  /pr feature/02-tests feature/01-core
  ```

Either way **the published branch is the layer's own history**, merges
included. Publication changes no gitlink.

## Not built

- **Linking the PRs as a stack.** `gh stack link --base main <pr-1> <pr-2>`
  needs no local branches; it waits on `std/github` (agent-github.md, "PRs").
  `gh stack push`, `submit` and `rebase` need local branches and stack
  metadata, which would mean reconstructing a repository from gitlinks and
  returning rewritten commits to caos. `modify` needs linear history, so it
  cannot restructure a stack with merge commits.
