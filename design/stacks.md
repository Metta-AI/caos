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

The `merge` std tool works like so: `merge(source_tree=Y, theirs=X)`, where
`Y` is the gitlink being merged into and `X` is a full commit hash. It always mints a merge commit
with parents `Y` and `X`. A conflict still moves along the gitlinik and introduces a a `.caos/conflicts` file listing items that need to be resolved. See chat.md's "Resolving source-tree conflicts" for info on how these are handled.

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
layer below, then merge in this layer's old tip with `base` set to the old tip
of the layer below it. **TODO:** `merge` has no `base` parameter yet
(`git merge-tree --merge-base`).

```text
rm -rf mystack/01-feature; cp -a mystack/00-base mystack/01-feature
merge(source_tree="mystack/01-feature", theirs="<A>", base="<H>")
rm -rf mystack/02-tests; cp -a mystack/01-feature mystack/02-tests
merge(source_tree="mystack/02-tests", theirs="<B>", base="<A>")
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

This takes three steps:

1. **Squash** with `caos-std/squash-layers`, called as
   `run_tool(path="caos-std/squash-layers", arguments={...})`:

   - `stack`: conversation path of the folder holding the layers, e.g. `mystack`.
   - `onto`: full commit hash the first layer goes on: `00-base`'s, `H`.
   - `messages`: conversation path of a folder holding one file per layer to
     publish, named after its entry. Each file is that layer's full commit
     message, used verbatim:

     ```text
     mystack-messages/01-feature    Add the feature\n\n<body>
     mystack-messages/02-tests      Test the feature\n\n<body>
     ```

   The layers are the message files, in filename order (byte order,
   `LC_ALL=C`), so `00-base` (which has none) is not published.

   For each layer it mints
   `C_i = commit(tree(layer i), parent C_{i-1} or onto, message_i)`, with the
   author and committer of the layer's tip, and returns them as one tree of
   gitlinks, `T = {01-feature: C1, 02-tests: C2}`, which the agent sees as
   `result tree <T>: 01-feature 02-tests`. The same input mints the same
   commits, so republishing an unchanged stack pushes nothing new.

   It refuses, as a `FAILED` report rather than an error, a layer that does
   not contain the one below it (or `onto`): merge first. It also refuses a
   message file naming no stack entry, or an entry that is not a gitlink, an
   empty message file or a subfolder, and a folder with no message files.

2. **Link** `T` into the conversation with the shell, since `publish_source`
   takes a gitlink. The call declares `paths: ["publish/mystack"]`: on a
   republish the old link is there, and the shell can only remove what it
   has materialized (undeclared, `rm -rf` is refused with `Permission
   denied`). On the first publish the path does not exist yet, and declaring
   it costs nothing; on a republish it checks out the old squashed layers
   only for `rm -rf` to delete them.

   ```sh
   caos get-hash <T> /cas/s; rm -rf publish/mystack; mkdir -p publish; ln -s /cas/s publish/mystack
   ```

   Each child, `publish/mystack/01-feature`, is then a gitlink in the
   conversation tree at `C1`, which is what `publish_source` takes as a
   `source_tree`. `tests/chat-squash-publish` runs steps 1 and 2 and calls
   `publish_source` on a child; the push itself is not tested.

3. **Push**, bottom to top, with `publish_source`:

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

