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

**Reviewers see one commit per layer**, so the agent collapses the stack every
time it publishes, the first publish and each update alike. The layers keep
their merge history; collapsing only mints commits beside them.

```text
feature/01-core  -> A'  (merge history)   C1 = tree(A'), parent H    -> feature/01-core
feature/02-tests -> B'  (merge history)   C2 = tree(B'), parent C1   -> feature/02-tests
```

1. **Collapse.** `caos-std/collapse-stack` with `stack=feature`, `onto=H` (the
   base tip the bottom layer last merged) and one `<entry> <message>` line per
   layer, bottom first. It mints
   `C_i = commit(tree(layer i tip), parent C_{i-1} or H, message_i)` with the
   author and committer of layer i's tip, and prints `<layer> <C_i>`. It
   refuses, naming both, a layer that does not contain the one below it (or
   H): merge first. Identical input mints identical commits, so republishing
   an unchanged stack pushes nothing new.
2. **Link.** `publish_source` publishes a gitlink, so each `C_i` goes into the
   conversation: `caos get-hash C_i /cas/c; rm -rf publish/<layer>; ln -s
   /cas/c publish/<layer>`. A linked commit is a directory, hence `rm -rf`.
3. **Push, bottom to top,** with `publish_source(rewrite=true)`
   (design/agent-publish.md). A collapsed commit does not descend from the one
   it replaces, so this is the one case for a rewrite. **The push stays leased
   on the exact remote head observed when the call starts**: a rewrite can
   replace history the agent has seen, never a change someone else pushed.

The first PR targets the base branch; each later one targets the branch below
it, which must already exist remotely.

**The TUI's `/pr` does not collapse.** `/pr <layer> <base-branch>`, in the same
order, pushes the layer's own history, merges included (chat.md, "Publishing
with `/pr`"); when the source does not contain the base's tip, its preview
offers to import it and ask the agent to merge it up instead. It is slated for
removal once agent publication covers it (agent-github.md, "PRs").

## Not built

- **Linking the PRs as a stack.** `gh stack link --base main <pr-1> <pr-2>`
  needs no local branches; it waits on `std/github` (agent-github.md, "PRs").
  `gh stack push`, `submit` and `rebase` need local branches and stack
  metadata, which would mean reconstructing a repository from gitlinks and
  returning rewritten commits to caos. `modify` needs linear history, so it
  cannot restructure a stack with merge commits.
