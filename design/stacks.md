# Stacks

A stack is an ordered sequence of refs, or layers, that share git ancestry.

In caos they are represented by a folder containing sibling gitlinks, each of which
corresponds to one layer of the stack.


```text
mystack/00-base     -> H
mystack/01-feature  -> A   parent H
mystack/02-tests    -> B   parent A
```

## Layers

- Their order is inferred by their names

- They are understood to descend directly from each other, in that order. If they don't, merges from parents (lower-numbered layers) into children (higher-numbered) may be necessary.

- Agents can start a new stack doing something like the following:
  ```sh
  cp -a imports/repo/main mystack/00-base  # assuming this is a gitlink file
  cp -a mystack/00-base mystack/01-feature
  ```

  And subsequent edits to the source tree `mystack/01-feature` will be reflected

  in updates to its git history (and the git hash kept in that file)

## Examples of common operations on stacks

Every operation below is made of three things: editing a layer, merging a
lower layer into a higher one, and `cp -a` / `mv` / `rm -rf` on the gitlinks
themselves. 

The `merge` std tool works like so: `merge(source_tree=Y, theirs=X, merge-base=B)`, where
`Y` is the gitlink being merged into, `X` is a full commit hash, and the optional `B` overrides the merge base. It always mints a merge commit
with parents `Y` and `X`. A conflict still moves along the gitlink and introduces a `.caos/conflicts` file listing items that need to be resolved. See chat.md's "Resolving source-tree conflicts" for info on how these are handled.

All examples below assume we're starting with:

```text
mystack/00-base     -> H
mystack/01-feature  -> A   parent H
mystack/02-tests    -> B   parent A
```

### Changing a lower layer

Edit `mystack/01-feature`, progressing it to A2, then merge each layer into the one above it.

```text
merge(source_tree="mystack/02-tests", theirs="<full commit hash of mystack/01-feature>")
```

`std/merge/worker` will choose `A` as the base for the merge (it looks for the highest common ancestor). After resolving conflicts, the result will look like:

```text
mystack/01-feature  -> A2  parent A
mystack/02-tests    -> B2  parents B, A2
```


### Updating the base

Import the new upstream tip `H2` at a fresh path, put it at `00-base`, and merge
up:

```sh
rm -rf mystack/00-base
cp -a imports/repo/main-H2 mystack/00-base
```

```text
merge(source_tree="mystack/01-feature", theirs="<full commit hash of mystack/00-base>")
... # potentially resolve conflicts
merge(source_tree="mystack/02-tests", theirs="<full commit hash of mystack/01-feature>")
```

Resulting in:
```text
mystack/00-base     -> H2
mystack/01-feature  -> A2  parents A, H2
mystack/02-tests    -> B2  parents B, A2
```

If `H2` descends from `H` (as it should unless the imported ref has been force-pushed over in the intervening time), then here, too, there are no rewrites: `mystack/01-feature` still descends from `H` because `H2` does.

If it was force-pushed, rebuild each layer bottom-up instead: copy the rebuilt
layer below, then merge in this layer's old tip with `merge-base` set to the old tip
of the layer below it.

```text
rm -rf mystack/01-feature; cp -a mystack/00-base mystack/01-feature
merge(source_tree="mystack/01-feature", theirs="<A>", merge-base="<H>")
rm -rf mystack/02-tests; cp -a mystack/01-feature mystack/02-tests
merge(source_tree="mystack/02-tests", theirs="<B>", merge-base="<A>")
```

### Drop the contents of a layer

Just a special case of editing.

Replace `01-feature`'s files with `00-base`'s, then merge up and delete the gitlink

```sh
find mystack/01-feature -mindepth 1 -delete
cp -R --preserve=mode mystack/00-base/. mystack/01-feature/
```

```text
merge(source_tree="mystack/02-tests", theirs="<full commit hash of mystack/01-feature>")

mystack/01-feature  -> A2  parent A          tree(A2) = tree(H)
mystack/02-tests    -> B2  parents B, A2     H + tests only
```

```sh
rm -rf mystack/01-feature
```

The merge base is `A`, so `A → A2` removes the feature from `02-tests` too.

We dont need to relabel mystack/02-tests to mystack/01-tests; gaps are fine.


### Combining two layers

Delete the lower one; the upper one already contains it:

```sh
rm -rf mystack/01-feature
```

### Splitting a layer in two

Suppose `01-feature` holds changes X and Y. Make a new layer for X from the layer below,
and keep the original as the layer for Y:

```sh
mv mystack/02-tests   mystack/03-tests
mv mystack/01-feature mystack/02-y
cp -a mystack/00-base mystack/01-x
# then re-make X in mystack/01-x
```

```text
merge(source_tree="mystack/02-y", theirs="<full commit hash of mystack/01-x>")
merge(source_tree="mystack/03-tests", theirs="<full commit hash of mystack/02-y>")

mystack/01-x        -> X1  parent H
mystack/02-y        -> A2  parents A, X1
mystack/03-tests    -> B2  parents B, A2
```

## Publishing

The git history from the working stack will have lots of commits, for agent turns, merges, etc. So the publishing process squashes each layer's changes into a single commit, forming a clean new temp stack.

```text
mystack/01-feature  -> A'  (merge history)   C1 = tree(A'), parent H    -> publish/mystack/01-feature
mystack/02-tests    -> B'  (merge history)   C2 = tree(B'), parent C1   -> publish/mystack/02-tests
```

This takes two steps:

1. **Squash** with `caos-std/squash-stack`, called as
   `run_tool(path="caos-std/squash-stack", arguments={"plan": "mystack.plan"})`.
   The plan is one file, one `key=value` per line:

   ```text
   onto=mystack/00-base
   into=publish/mystack
   layer=01-feature
   commit=mystack/01-feature
   message=Add the feature
   message=
   message=<body>
   layer=02-tests
   commit=mystack/02-tests
   message=Test the feature
   ```

   `onto` and each `commit` are a conversation path to a gitlink or a full
   commit hash. Each `layer=` starts a block that becomes `<into>/<layer>`; its
   `message=` lines, in order, are its commit message.

   For each layer it mints
   `C_i = commit(tree(commit i), parent C_{i-1} or onto, message_i)`, with the
   author and committer of that commit, and writes them to `into` as one
   folder of gitlinks, `{01-feature: C1, 02-tests: C2}`. It refuses a layer
   that does not contain the one before it (or `onto`): merge first. The same
   plan mints the same commits, so republishing an unchanged stack pushes
   nothing new.

2. **Push**, bottom to top, with `publish_source`:

   - `source_tree`: the gitlink to publish, e.g. `publish/mystack/01-feature`.
   - `repository`: HTTPS repository URL, without credentials.
   - `branch`: destination branch, without `refs/heads/`.
   - `rewrite` (optional): `true` to allow a non-fast-forward update.

   It pushes exactly that commit to the branch, and creates no PR. Updates are
   fast-forward only unless `rewrite=true`; a squashed commit does not descend
   from the one it replaces, so updating a stack always needs it. Even then the
   push is leased on the remote head observed when the call starts: it can
   replace history the agent has seen, never a change someone else pushed
   (design/agent-publish.md).

This publishes branches, not PRs. **TODO:** open and update a PR per layer,
each based on the branch below (the first on the base branch), and then remove
the TUI's `/pr` (chat.md, "Publishing with `/pr`"), which pushes a layer's
unsquashed history.

## Not built

- **Linking the PRs as a stack.** `gh stack link --base main <pr-1> <pr-2>`
  needs no local branches; it waits on `std/github` (agent-github.md, "PRs").
  `gh stack push`, `submit` and `rebase` need local branches and stack
  metadata, which would mean reconstructing a repository from gitlinks and
  returning rewritten commits to caos. `modify` needs linear history, so it
  cannot restructure a stack with merge commits.

