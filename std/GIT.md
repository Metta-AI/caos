# Git in caos: importing, editing, stacks, publishing, PRs

Everything an agent needs to get code in from GitHub, change it, and get it
back out as a branch or a stack of PRs. The design rationale lives in
`design/` (linked at the end); this file is the working guide.

## The model in one paragraph

Code is never checked out. A **source tree** is a gitlink in the conversation
tree (a path such as `feature/01-change`) that points at a commit. `import_source`
creates one from GitHub, `copy` duplicates one with its history, `write`/`edit`
(and `bash-tool`) change one, and every accepted change records a child commit
automatically: there is no staging and no commit step. `log`, `show` and `diff`
are how you see history, because there is no `git` in the shell. Getting
changes out is two separate things: `publish_source` pushes a commit to a
branch, and `caos-std/github` does everything else on GitHub (open a PR,
change its base, link a stack).

## Tools at a glance

| to | use |
|---|---|
| bring in a repo at a branch, ref or full commit hash | `import_source` |
| duplicate a source tree, with its history | `copy` |
| change files | `write`, `edit` |
| see history and changes | `log`, `show`, `diff` |
| merge another commit into a source tree | `caos-std/merge` (or the registered `merge` tool) |
| squash a stack to one commit per layer | `caos-std/create-squashed-stack` |
| push a commit to a GitHub branch | `publish_source` |
| open a PR, change its base, link a stack, read PR state | `caos-std/github` |

`tool_help(path="caos-std/<name>")` prints a std tool's exact parameters.

## One change, one layer

```text
import_source(source="https://github.com/<owner>/<repo>.git",
              revision="main", into="imports/<repo>/base")
copy(from="imports/<repo>/base", to="feature/01-change")
... edit feature/01-change ...
publish_source(source_tree="feature/01-change",
               repository="https://github.com/<owner>/<repo>.git",
               branch="my-branch")
run_tool(path="caos-std/github", arguments={...})   # open the PR
```

- Leave `imports/<repo>/base` untouched. It is the record of where you
  started, and what later diffs and merges are measured against.
- Edit and publish a source tree (`imports/...`, `feature/...`), never the
  conversation root. `edit` on a bare path succeeds but changes only scratch
  files, and `publish_source` rejects `.`.
- Omit `revision` to import the default branch. A new `import_source` call
  observes the remote again, so importing the same branch later at a fresh
  path gives you its new tip. Public repositories need no token.
- The destination path and its `.source.json` sibling must both be unused.
  Use a fresh path per import (`main`, `main-2`, or the commit hash).
- Read the imported repo's own `AGENTS.md` / `CLAUDE.md` before editing: they
  govern that code.

## Stacks

A stack is an ordered sequence of layers that share git ancestry, so a change
can be reviewed as several small PRs, each built on the one below. In caos a
stack is a **folder of sibling gitlinks**, one per layer:

```text
mystack/00-base     -> H
mystack/01-feature  -> A   parent H
mystack/02-tests    -> B   parent A
```

- Order comes from the names. Gaps are fine (`00`, `01`, `03`).
- Each layer descends from the one below it. When it does not, merge the lower
  layer into it (see below).
- `00-base` is the upstream tip the stack builds on. It is what the first PR
  is based on.

Start a stack by copying:

```text
copy(from="imports/repo/main", to="mystack/00-base")
copy(from="mystack/00-base",   to="mystack/01-feature")
```

Edit `mystack/01-feature`; its history and gitlink advance on their own. To add
the next layer, `copy` the layer below it and edit the copy.

Every operation on a stack is made of three things: editing a layer, merging a
lower layer into a higher one, and `copy` / `move` / `remove` on the gitlinks.
Examples below start from the three-layer stack above.

### Merging

`merge(source_tree=Y, theirs=X)` merges the full commit hash `X` into the
gitlink `Y` and always mints a merge commit with parents `Y` and `X`. It picks
the highest common ancestor as the base. The optional `merge-base` overrides
that, which you only need to replay a change onto a history that lacks its old
base (a layer rebuilt on a force-pushed upstream: `merge-base` is the old tip
of the layer below).

`theirs` must be a **full commit hash**. Get it from `log` on the source tree.

On a conflict the merge still advances the gitlink, with git's inline markers
in the files and a reserved `.caos/conflicts` file listing every unresolved
path, including conflicts that have no markers (delete/modify, mode, binary).
Resolve each path: edit the file (use `read` with the stage's oid as `root` to
inspect a stage), then delete that path's rows from `.caos/conflicts`. Saving
an empty ledger removes it. A ledger left in a published commit counts as an
unresolved conflict, so clear it before you publish, and run the repo's checks
after resolving.

### Changing a lower layer

Edit `mystack/01-feature` (A becomes A2), then merge each layer into the one
above it, bottom to top:

```text
merge(source_tree="mystack/02-tests", theirs="<full hash of mystack/01-feature>")

mystack/01-feature  -> A2  parent A
mystack/02-tests    -> B2  parents B, A2
```

### Updating the base

Import the new upstream tip at a fresh path, replace `00-base`, merge up:

```text
import_source(source=..., revision="main", into="imports/repo/main-H2")
remove(file-path="mystack/00-base")
copy(from="imports/repo/main-H2", to="mystack/00-base")
merge(source_tree="mystack/01-feature", theirs="<full hash of mystack/00-base>")
merge(source_tree="mystack/02-tests",   theirs="<full hash of mystack/01-feature>")
```

If `H2` descends from `H` nothing is rewritten. If upstream was force-pushed,
rebuild each layer bottom-up instead: copy the rebuilt layer below, then merge
this layer's old tip in with `merge-base` set to the old tip of the layer
below.

```text
remove mystack/01-feature; copy mystack/00-base -> mystack/01-feature
merge(source_tree="mystack/01-feature", theirs="<A>", merge-base="<H>")
remove mystack/02-tests;   copy mystack/01-feature -> mystack/02-tests
merge(source_tree="mystack/02-tests",   theirs="<B>", merge-base="<A>")
```

### Dropping, combining, splitting

- **Combine two layers:** `remove` the lower one. The upper already contains
  it.
- **Drop a layer's contents:** replace `01-feature`'s files with `00-base`'s,
  merge it up into `02-tests` (the merge base is `A`, so the removal carries
  up), then `remove` the `01-feature` gitlink. Do not renumber `02-tests`; gaps
  are fine.
- **Split a layer in two:** `move` `02-tests` to `03-tests` and `01-feature`
  to `02-y`, `copy` `00-base` to `01-x`, re-make X in `01-x`, then merge
  `01-x` into `02-y` and `02-y` into `03-tests`.

### Harvesting from subagents

`harvest_agent` applies a child agent's changes to the parent's source at their
existing paths. It creates no stack layers: the parent decides the stack's
shape. If reconciliation conflicts, the proposal is retained for resolution
rather than partly installed.

## Publishing

The conversation history has many commits (agent turns, merges). Publishing
that as it is works (`publish_source` takes any gitlink), but squashing first
gives each layer one clean commit and linear history.

### 1. Squash (optional but usual for a stack)

Write a plan file, one `key=value` per line. `onto` and `into` come once,
before the first `layer=`:

```text
onto=mystack/00-base
into=publish/mystack
layer=01-feature
commit=mystack/01-feature
message=Add the feature
message=
message=Why, in a body.
layer=02-tests
commit=mystack/02-tests
message=Test the feature
```

```text
run_tool(path="caos-std/create-squashed-stack",
         arguments={"plan": "mystack.plan", "in": "<tree to run over>"})
```

`onto` and each `commit` are a conversation path to a gitlink or a full commit
hash. Each `layer=` block becomes `<into>/<layer>`; its `message=` lines, in
order, are the commit message. Each squashed commit takes the tree of its
`commit`, the parent of the previous squashed commit (or `onto`), and the
author and committer of the original. The same plan mints the same commits, so
republishing an unchanged stack pushes nothing new. It refuses a layer that
does not contain the one before it (or `onto`): merge first. Pass `in` if the
tool's own source tree is not the one you mean (the default is the tree the
tool sits in).

### 2. Push, bottom to top

```text
publish_source(source_tree="publish/mystack/01-feature",
               repository="https://github.com/<owner>/<repo>.git",
               branch="mystack-01-feature", force=true)
publish_source(source_tree="publish/mystack/02-tests", ..., branch="mystack-02-tests", force=true)
```

- A squashed commit does not descend from the one it replaces, so updating a
  published stack always needs `force=true`. The push is still leased on the
  exact remote head observed when the call starts: it can replace history you
  have seen, never a change someone else pushed.
- Without `force`, only creates and fast-forwards succeed.
- `publish_source` creates no PR and does not change the source gitlink.
- It rejects any commit whose tree contains a path matched by the commit's own
  `.gitignore` rules, **including tracked files**. It never strips files or
  rewrites commits. Remove the files or fix the rules, then publish again.
  (Global excludes and `.git/info/exclude` do not apply, and only the
  published snapshot is checked, not earlier commits.)
- Resolve merge conflicts and clear `.caos/conflicts` first. The endpoint does
  not scan for the ledger or for inline markers.
- Test and inspect the intended PR diff (`diff`) before publishing.
- The HTTPS URL carries no credentials. Pushes go from the server's store,
  not from your container; nothing is checked out.

If a push comes back as uncertain or as a lease conflict, read the remote
branch before retrying (see "Recovering" below).

### 3. Open a PR per layer and link them

`caos-std/github` is one GitHub API call: `method`, `path` under
`https://api.github.com`, and optional JSON `body`. Every value is a string.
The result is the status line and the body, cut if long. A run fails only when
no response came back, and a write may still have arrived, so `GET` the state
before resending. The tool reaches only `api.github.com` and follows no
redirects. Paths are unrestricted; the token's scope is the boundary.

For each layer, bottom first:

1. `GET /repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=open`
2. If there is none, `POST /repos/{owner}/{repo}/pulls` with `head`, `base`
   (the trunk for the first layer, the branch below for the rest), `title` and
   `body` taken from the layer's `message=` lines.
3. If there is one, `PATCH` its `base` if it differs. Leave its title and
   body alone: a person may have edited them.

```text
run_tool(path="caos-std/github", arguments={
  "method": "POST", "path": "/repos/<owner>/<repo>/pulls",
  "body": "{\"title\":\"Add the feature\",\"head\":\"mystack-01-feature\",\"base\":\"main\",\"body\":\"Why.\"}"})
```

Then link them, bottom first:
`POST /repos/{owner}/{repo}/stacks` with `{"pull_requests": [<numbers>]}`, or
`POST .../stacks/{number}/add` to extend an existing stack. The chained bases
already give a reviewable stack without the link.

## Recovering

- **Uncertain push or lease conflict.** A remote can accept a push before the
  connection drops. Read the branch with `GET /repos/{owner}/{repo}/branches/{branch}`
  (or `import_source` it at a fresh path): if it is your commit, the push
  completed. If it moved to something else, another change landed; import it,
  merge, test, and publish again. Do not blindly retry.
- **A `github` write with no response.** It may still have arrived. `GET` the
  PR first. GitHub refuses a duplicate PR and a repeated base change is a
  no-op.
- **Remote changed under you.** `import_source` the new head at a fresh path
  and `merge` it in. Importing and merging are never hidden inside a push.

## Authentication

The GitHub token is a secret in the caos server's secret store, not part of
any repo. `import_source`, `publish_source` and `caos-std/github` all use it
(public imports need none). Credentials apply only to `github.com` on the
default HTTPS port and never appear in URLs, config, arguments or logs. A
scoped token only reaches the repositories it was granted, and `caos-std/github`
can do whatever that token allows, so a 403/404 on a repo the agent should
reach usually means the grant is missing.

## Pitfalls

- `theirs` for `merge` is a full commit hash, not a path or a branch name.
- `copy` and `move` refuse an existing destination; `remove` first.
- An import is an unchanged snapshot. It never merges into or advances
  another source tree.
- Ignored scratch files can ride along in a source during work, and then block
  `publish_source`. Clean them before publishing.
- `bash-tool` runs over a scratch copy: declare every existing path it reads
  in `paths`. Use `read`, `grep`, `copy`, `move` and `remove` for those jobs.
- Subagents have no file tools. Do any reading of a saved large result
  yourself.

## Further reading (design docs in the caos repo)

- `design/stacks.md` – the stack model and the full operation walk-through.
- `design/agent-publish.md` – `publish_source`: leases, `force`, `.gitignore`
  rejection, uncertain results.
- `design/agent-github.md` – import, `caos-std/github`, PRs for a stack, the
  token.
- `design/chat.md` – source-tree conflict ledgers, harvesting, publishing.
