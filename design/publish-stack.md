# Publishing a stack in one call: `publish-stack`, and `move-sed`

A stack (stacks.md) is a folder of sibling gitlinks, `sources/<repo>/`, with
the upstream tip at `00-base` and one layer per `NN-<name>`. Getting it onto
GitHub used to be a recipe the agent followed by hand: push each layer,
earliest first; find or open a PR per layer, each based on the branch below;
link the PRs as a stack. Measured on a two-layer stack, that was nine ordered
tool calls for the first publish and five to seven for every republish, each
PR response around 20 KB of JSON, and three traps along the way: `force`
forgotten, a duplicate PR opened because the lookup was skipped, and GitHub's
refusal to retarget a PR that is already in a stack. `publish-stack` is that
recipe as one operation. `move-sed` is the renaming tool that keeps a layer
and its description together when the stack changes shape.

## No squashing

Branches are merged into the trunk by squashing, so the commits inside a
branch never reach it: only the branch's net diff does. `publish-stack`
therefore pushes each layer's gitlink exactly as it is, merge commits and
"Edit source tree" commits included. Two things follow:

- A republish is a fast-forward. After the base moves, the agent merges the
  new `00-base` into the first layer and each layer into the next
  (stacks.md, "Updating the base"); every new tip descends from the published
  one, so no push needs `force`. A non-fast-forward push means a layer was
  rebuilt (a force-pushed upstream) or someone else pushed to the branch, and
  the agent should decide which, so `force` is a parameter and is off by
  default.
- Nothing mints commits. `create-squashed-stack` and its plan file are not
  part of this flow, and the authorship drift it had (a squashed commit copied
  the author of whatever commit happened to be the layer's tip) does not
  arise.

## The stack directory is the specification

```text
sources/<repo>/
  00-base                 the upstream tip, exactly as imported; never published
  00-base.source.json     provenance: the repository and the branch imported
  01-parser               a layer: a gitlink
  01-parser.pr.md         its PR: title on the first line, body after it
  02-tests
  02-tests.pr.md
  99-dirty                scratch: no description, so not published
```

- **Layers** are the gitlinks named `NN-<name>` with two leading digits and a
  dash, in name order. Entries named `00-*` are never published: `00-base` is
  what the first layer builds on, and the first layer's PR targets the trunk.
- **Branch names** drop the number: layer `01-parser` goes to the branch
  `parser`, or `<branch-prefix>parser` when a prefix is given. The number
  orders the folder and nothing else, so renumbering a layer (stacks.md,
  "Splitting a layer in two") keeps its branch and its PR.
- **Descriptions** live beside the gitlink as `NN-<name>.pr.md`, the same
  shape as `00-base.source.json`: the first non-empty line is the PR title
  (a leading `# ` is dropped), everything after it is the body, and both are
  taken verbatim. The file is the description's home: when a PR's title or
  body differs from it, the operation updates the PR and says so in its
  report, so an edit made on GitHub is overwritten on the next publish. The
  rule used to be the other way round (leave PR text alone, a person may have
  edited it); with the agent owning the stack, the conversation is where the
  text is edited.
- **A layer without a description is not published.** That is how `99-dirty`
  is left out without a special case. A described layer ABOVE an undescribed
  one is refused, naming the layer that needs a description: publishing it
  would base its PR on a branch that does not exist.
- **Repository and trunk** default to `00-base.source.json`: `repository` as
  imported, and the trunk is the branch that was requested, or the remote's
  default branch when none was. Both can be given explicitly, which is also
  how a stack built on an imported commit hash names its trunk.

The description is NOT a file inside the layer's tree. That would put it in
the PR's diff and on the trunk, or require rewriting the commit to strip it,
which is the squashing this design does without.

## The operation

`publish-stack` is an operation of the step (`@op`, SPEC "CaosTools"), like
`publish-source`, and for the same reason: a publication is pinned in the
conversation before any remote write (agent-publish.md), and only the step
writes conversation records. Its entry is `std/publish-stack`, reached as
`caos-std/publish-stack`; the work is `std/llm-step/src/publish_stack.rs`.

```text
run_tool(path="caos-std/publish-stack", arguments={
  "stack": "sources/repo",           # the stack directory
  "branch-prefix": "malcolm/",      # optional
  "repository": "https://...",      # optional: default from 00-base.source.json
  "trunk": "main",                  # optional: default from 00-base.source.json
  "force": "true",                  # optional: allow non-fast-forward pushes
  "link": "false",                  # optional: do not link the PRs as a GitHub stack
  "draft": "true",                  # optional: open new PRs as drafts
  "dry-run": "true"})               # optional: report the plan, change nothing
```

One call does, in order:

1. **Read the stack.** List the directory, pair gitlinks with descriptions,
   parse the descriptions, derive the branch names and check them
   (`validate_branch`). Every refusal in this step and the next happens before
   anything is observed or pushed, and names the entry at fault.
2. **Check ancestry.** Each layer's commit must contain the one below it
   (`00-base` for the first). A layer that does not is refused with the merge
   to run: `merge(source-tree=<layer>, theirs=<layer below>)`. This is the
   check `create-squashed-stack` made at squash time, now made where the
   stack is published.
3. **Observe the remote** head of every branch (`ls-remote`, as
   `publish-source` does), and **pin** one publication record per layer in
   the conversation, all in one append with the call's start, so a resumed
   attempt sends exactly what the first one planned: the same commits against
   the same leases.
4. **Push bottom to top** with `caos push-git`, each push leased on its
   observed head. A rejected or uncertain push stops the loop: the layers
   above would have been based on a branch that did not move. Each outcome is
   recorded as the publication's terminal transition as soon as it is known.
5. **Find or open each PR**, bottom to top, when the repository is on
   GitHub (`api.github.com`, the same `github-token` secret `std/github`
   uses). `GET /pulls?head=<owner>:<branch>&state=open`; none means `POST
   /pulls` with the chained base (the trunk, or the branch below), the
   description's title and body, and `draft` when asked. An existing PR
   whose title or body differs from the description is `PATCH`ed; one whose
   base branch differs is `PATCH`ed too, unless it is in a GitHub stack (see
   below).
6. **Link the PRs as a stack**, unless `link=false`. The PR objects already
   say which stack each is in and at what position (`stack.number`,
   `stack.position`). If every PR is in one stack, in this order, from the
   bottom, nothing is done. If the bottom PRs are that stack and the rest are
   in none, `POST /stacks/{n}/add` appends them. Anything else is a shape
   change: `POST /stacks/{n}/unstack` for every stack involved, then the
   deferred base `PATCH`es, then `POST /stacks` with the whole ordered list.
7. **Report**, one line per layer, then the stack:

   ```text
   repository https://github.com/owner/repo  trunk main
   01-parser  parser  3f9c2e1d  pushed (fast-forward)  PR #365 created, base main
   02-tests   tests   8b11d0aa  unchanged              PR #366 unchanged
   stack 367: unchanged
   ```

   A refusal or a failed push is an error result that says what was done and
   where it stopped, so the model knows which branches moved.

### Why GitHub stacks are linked on every publish

Chained bases alone give a reviewable diff per PR (a PR diffs against its
base branch). The link matters for the MERGE: the trunk squash-merges, so
once the bottom PR lands its commits are not in the next layer's ancestry,
and an unlinked PR retargeted to the trunk would show the lower layer's
changes again. Merging a stacked PR through GitHub's stack merges every PR up
to it, atomically. So the operation links by default.

The cost is that GitHub refuses to change the base BRANCH of a PR while it is
in a stack (`422 Cannot change the base branch because the pull request is
part of a stack`), which is why a shape change goes through `unstack` first.
The base COMMIT moves freely: after the agent merges a new trunk tip into the
layers and republishes, GitHub reads the new bases from the branches, with no
API call. The stack API also has no reorder and `add` only appends, so every
other change is unstack-and-recreate, which gives the stack a new number.
PRs queued for merge cannot be unstacked; the operation reports that rather
than failing the pushes, which already happened. The stacks API is in public
preview and may change; everything about it is in one place, the stack step.

### Resuming

A started call carries its pin (`publish-stack.json` in the call's payload
directory: the stack's layers, branches and the publication ids, in order).
A resumed attempt re-reads the publications: a Pending one is pushed exactly
as pinned, a terminal one is not sent again. The GitHub steps are repeated
whole: each is a read before a write, GitHub refuses a duplicate PR, and the
stack step compares before it changes anything, so a crash between the
pushes and the report costs a few reads.

### What it does not do

- Merge a layer into the one above it. A missing merge is a refusal that
  names the call, because a merge can conflict and resolving one is the
  agent's work.
- Squash, rewrite or mint commits, or delete branches.
- Merge PRs. That is a reviewer's decision; GitHub's asynchronous merge API
  for stacks is the next tool, not this one.
- Publish to a remote it cannot lease on. The pushes have the same guards as
  `publish-source`: fast-forward unless `force`, `.gitignore` check unless
  `allow-ignored`, a `.caos/conflicts` ledger is the agent's to clear first.

## `move-sed`: renaming a layer and its description together

`copy`, `move` and `remove` act on one path, and a layer is two: the gitlink
and its `.pr.md` (and for `00-base`, its `.source.json`). Renumbering a stack
or renaming a layer means moving both, and two `move` calls are two
proposals, each checked against a tree the other may have changed.
`move-sed` renames the entries of ONE directory by a regular expression, in
one proposal:

```text
run_tool(path="caos-std/move-sed", arguments={
  "dir": "sources/repo",
  "pattern": "^01-feature(\\..+)?$",
  "replacement": "02-y$1"})
```

- `pattern` is a Go regular expression (RE2), matched anywhere in an entry's
  name as sed's `s` does; anchor it with `^` and `$` to match the whole
  name. `replacement` uses Go's `$1` and `${name}`; sed's `\1` is refused
  with the correction, because Go would write it literally and the file
  would be called `\1`.
- Every matching entry is renamed; non-matching entries are untouched.
  Nothing matching, two entries mapping to one name, a new name that is
  empty, `.`, `..`, contains `/`, or is `.caos` at the root, and a
  destination that exists and is not itself being renamed are all refusals,
  before anything changes. A directory inside a source tree is refused too:
  renaming files in code is `move`'s job, and this tool rebuilds conversation
  trees, not commits.
- The result lists every rename, old name to new, and `dry-run=true` lists
  them without renaming.

It is a writer on the conversation (`@writer`, `@in`), a Go program on
`std/go` like `create-squashed-stack`, and its proposal is the conversation
with that one directory rebuilt. The whole rename is one tree, so a shift
such as `02-tests` to `03-tests` and `01-feature` to `02-y` in one call has
no ordering problem. Regex replacement cannot count, so renumbering a run of
layers is one call per layer; stacks are short.

## Tests

- `tests/publish-stack`: llm-step's real tool loop with a scripted model,
  the shape of `tests/chat-squash-publish`. It builds a stack with
  descriptions in a conversation and calls `caos-std/publish-stack` against
  a `.invalid` repository, so no push happens: the call must get through
  every check and stop at reading the remote; a stack with a described layer
  above an undescribed one, and one whose upper layer lacks the lower, must
  be refused before the remote is read; `dry-run` must report the plan.
- `tests/move-sed`: a worker test over a fixture conversation, the shape of
  `tests/create-squashed-stack`: a success is checked by the exact tree it
  proposes, a refusal by an unchanged proposal and its message.
- Unit tests in `publish_stack.rs` for the pure parts: reading a stack
  directory, parsing a description, branch names, the GitHub stack
  reconciliation decision.

## Open

- Merging through the stack API after review, and deleting the branches of
  merged layers, are the next operations in this family.
- Secret grant: the operation runs in `llm-step`, which already holds the
  `github-token` reader, so no new grant is needed. A tool that did this
  work as a separate worker would need its own.
