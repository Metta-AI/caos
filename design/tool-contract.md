# One tool contract: `root`, writers, `request-id`, and no automatic resume

**Status:** proposed. Nothing here is built.

This replaces four overlapping mechanisms with one contract that every std
tool, including the git operations, can meet as an ordinary worker:

1. The step stops resuming calls by itself. A call that did not finish is
   answered as such and the agent decides what to do.
2. The git `@op` tools (`import-source`, `publish-source`, `stack-publish`,
   `stack-sync`, `stack-merge-to-master`, `ci-status`) become ordinary workers.
3. Every tool that reads or changes a tree names it with one argument, `root`.
   A writer returns a new object for `root`; the harness rewrites everything
   above it.
4. `@call` and the `request-id` convention become one tag, `@request-id`,
   filled in by the harness when the caller omits it.

The four depend on each other: (2) is only simple once (1) holds, (2) needs (3)
for `stack-sync` and (4) for every tool that talks to a remote.

## Why

### The `@op` tools cannot be changed like other tools

An `@op` entry (`std/stack-publish/.caos-expr` and the rest) is a stub: it
gives the operation a path and a help, and its curried image is never run.
When `run_tool` reads `@op <name>` in the help, the step that is already
running calls its own compiled-in function (`run_op` in
`std/llm-step/src/main.rs`). That step is fixed when the session starts: the
pinned `caos-std/llm-step`, or the dev commit in dev mode.

So editing `stack_publish.rs` in a copy of caos and running
`sources/caos/99-dirty/std/stack-publish` runs the OLD code. The copy's help is
read; its llm-step is never built. Every change to these operations needs a
merge, a release and a new session, while every other std tool can be tried
from a copy on the next call.

The reasons recorded for making them operations (`std/README.md`,
`design/stack-publish.md`, `design/agent-publish.md`) were:

- **Records before side effects.** `import-source` pins what it observed on
  the remote before transferring anything; `publish-source` appends a
  `PublicationPending` record (planned head H, expected old head E) before it
  pushes, then a `PublicationTerminal`. Only the step appends conversation
  records, and a worker gets exactly one write, at the end.
- **Atomicity.** `stack-sync` moves every layer's gitlink in one append.
- **The token.** `llm-step` already holds the `github-token` reader; a worker
  would need its own grant.

The first exists only to make AUTOMATIC RESUME of a side-effecting call safe.
When the step is restarted for a turn, `drive_call` finds a call left
`Started` and runs it again, unseen by the agent. Pure workers survive that for
free (the same content-addressed task is re-sent). An operation has no task to
rejoin, so it pins its inputs and re-sends exactly them. Without automatic
resume the pins have no job:

- an import that is retried by the agent re-resolves the revision, which is
  harmless because the first attempt returned nothing;
- a non-force publish is idempotent (a push that landed is a no-op; a push
  someone else overtook fails the ancestry check);
- the only real hazard, a FORCE push retried much later with a fresh lease, is
  removed by passing the expected head as an argument (below), which is a
  stronger guarantee than today's: the current lease is read by the tool
  milliseconds before it pushes, not by the agent when it reviewed the branch.

The receipts are read by nobody else: outside tests, `PublicationRecord` is
used only by `publish_source.rs` and `stack_publish.rs`, for their own resume.

The second is not a reason for an operation once a writer can return a
directory of gitlinks (below). `stack-publish` never needed it: it changes no
tree. The third disappears if every remote action goes through `std/github`,
which already has its own grant (section 2).

### The writer contract is harder than it needs to be

Today a writer's SCOPE (the tree it changes) is chosen three ways, in order:
the source tree an `@scope <param>` argument names; else the source tree the
tool's path sits in; else the whole conversation. The input tree is `in`, bound
only with `@in` and overridable by the caller; `@git` adds the input commit as
`wc`; the result is `{prop, out, message?, failed?}` where `prop` is a tree or,
for `merge` only, a commit that must descend from the base.

What agents trip over:

- **The same path means different things.** A writer under `caos-std/` takes
  conversation-relative paths; a repository tool inside a source tree takes
  paths relative to that tree. Nothing in the call says which.
- **Writers offer `in` and cannot honour it.** Every std writer declares `@in`,
  so `tool_help` lists an `in` parameter (with the hint that the default "is
  usually not the tree you mean"), but the writer's base stays pinned to the
  scope. Passing `in=sources/x/99-dirty` to `caos-std/edit` edits that subtree
  and then diffs a subtree-rooted result against the whole conversation tree.
  Nothing refuses it (`split_input`). Untried; read from the code.
- **One idea, five spellings:** `source-tree`, `source_tree`, `in`, the path
  prefix, and a source tree embedded in `file-path`.
- **Scope is one source tree or everything.** A tool that moves several
  gitlinks (`stack-sync`) fits only conversation scope, and therefore reads the
  whole conversation.

### Four ways to defeat the cache

| mechanism | defined in | set by | what it re-runs | users |
|---|---|---|---|---|
| `salt` | the ArgTree (SPEC, "Work"); reserved | the operator, `CAOS_SALT` | every caos worker; flake builds keep theirs | operators |
| `request-id` | SPEC: an ordinary arg "the server gives no meaning" | the caller | that request | `std/actor`, caos-stack |
| `@call` | SPEC, "Help text" | the harness binds the call id as `call` | that call | `std/github` only |
| `test-salt` | `std/caos-test` | the caller | the tests, not the build | `caos-test` |

`@call`'s second property, that the id is the same when a call is resumed so a
resumed call gets its stored result, goes away with automatic resume.

## 1. No automatic resume

The step never runs a call a second time. A call it finds `Started` from a
previous attempt at the turn is completed as failed with "this call did not
finish", the same answer the transcript already renders for such a call, and
the turn continues. The agent decides whether to call again.

Re-issuing a pure call is cheap: the same tool with the same arguments is the
same ArgTree, so it joins the running job or hits the cache. A tool that
declares `@request-id` gets a new id on the new call (below) and runs again,
which is the point.

What goes: the resume paths in `drive_call` and `run_op`, the `op.json`
marker, the import and publication pins, and the `PublicationPending` /
`PublicationTerminal` transitions once nothing writes them. Subagent and async
operations (`agent-spawn`, `agent-wait`, `agent-harvest`, `run-async`) keep
their own durable state; they are out of scope here (see "Not changing").

## 2. The git operations become workers

Each becomes an ordinary std tool built from source beside its entry, like
`std/log`, `show` and `diff` build `std/git-history`. Running it from a copy
of caos then runs the copy's code.

| tool | contract | notes |
|---|---|---|
| `import-source` | writer, `root` = the stack directory `sources/<repo>`; `into` names the entry; `@request-id` | writes the gitlink and its `.source.json` beside it; refuses if either exists |
| `publish-source` | reader, `root` = the source tree's gitlink; `@request-id` | pushes with `caos push-git`; without `force` it reads the remote head itself (the ancestry check makes that safe); with `force`, `expected` is required |
| `stack-publish` | reader, `root` = the stack directory; `@request-id` | per layer as `publish-source`; with `force`, `expected` lists `<branch> <oid>` per layer, as `dry-run` prints them |
| `stack-sync` | writer, `root` = the stack directory; `@request-id` | returns the directory with every layer's gitlink moved: one change, all or nothing |
| `stack-merge-to-master` | reader, `root` = the stack directory; `@request-id` | GitHub API calls only |
| `ci-status` | reader, `root` = the stack directory or none; `@request-id` | GitHub API calls only |

The result text is the receipt. Each tool reconciles an unclear push itself
(read the branch: H means it landed) before it returns, as `publish_source.rs`
does today in `reconcile`.

### One grant: everything remote goes through `std/github`

None of these tools holds the token. Each mounts its NEIGHBOUR,
`DEEP-DEPS/github` (the `std/github` beside it, not `caos-std/github`), and
hands it every remote action. The token is granted to `std/github` alone, by
the `reader:` line that already exists, so one grant covers every git tool,
however many there are.

Why that is enough:

- A grant matches a node's origin, not the path it is reached by, and a
  `DEEP-DEPS/<name>` mount is a copy, which "carries the origins of what it was
  copied from" (SPEC, "Readers"). The `@op` entries already mount
  `DEEP-DEPS/llm-step` this way.
- A secret is injected only into a job whose own image was granted (SPEC,
  "Isolation"). So the token is used inside the github worker and nowhere
  else; the other tools never see it.
- It exposes nothing new: the agent can already make any call the token allows
  through `caos-std/github`.

`std/github` becomes the one door to GitHub. It takes a BATCH of operations,
runs them in order, stops at the first failure unless told otherwise, and
returns one result per operation:

| op | what | today in |
|---|---|---|
| `api` | one GitHub API call: method, path, optional JSON body | `std/github` |
| `push` | `caos push-git <repo> <commit> <branch> --expected=<oid\|absent> [--force] [--allow-ignored]` | `publish_source.rs` |
| `import` | resolve a revision on the remote, then `caos import-git <repo> <commit>` | `import_source.rs` |
| `head` | read one remote branch's head, for leases and reconciliation | `publish_source.rs` |

It must stay a CLOSED program: its arguments are data, never code. A grant
also covers a job whose arg tree is a superset of the granted image (SPEC,
"Isolation"), so an image that ran something a caller curried onto it would
hand that caller the token.

A worker may not block (design/map-then.md), so a tool reaches github with
`caos run-request-then`: it records a continuation naming the github request
and itself as `then`, exits, and is run again with github's result. Batching
keeps the number of round trips small and independent of the stack's height.
`stack-publish`, for example:

1. read every layer's remote head and list the open PRs for their branches;
2. push every layer (stopping at the first failure);
3. create or update the PRs;
4. link them as a GitHub stack.

This also serves the reason the operations are moving at all. A `reader:@@=`
grant matches content, so editing `stack-publish`'s logic in a copy of caos
should leave `std/github` unchanged and still granted, and the edited tool
runs with the token. Only an edit to `std/github` itself needs main, or a
session grant (`reader:@=<path> session=<id>`, which exposes the token to that
session). Compiling all the logic into one granted image instead (the
`log`/`show`/`diff` pattern) would lose the grant on every logic edit made in
a copy.

Workers already carry `/bin/caos` (flake-builder adds it to every image), so
the github worker can run `caos push-git` and `caos import-git`; that the
server accepts `/git/push` and `/git/import` from a worker is to be checked
first.

## 3. `root`

### The argument

A tool that reads or changes a tree declares `@root`; `@writer` implies it.
The harness then lists `root` as a REQUIRED parameter: a path relative to the
conversation root. There is no default. In particular a repository tool that
sits inside a source tree does not default to that tree: where a tool sits
decides which definition runs, never what it runs over. `caos-test` is always
`caos-std/caos-test` with `root=sources/<repo>/<layer>`.

`eval_path` already uses `root` for the object it evaluates within, so the word
has one meaning everywhere.

The object at `root` arrives as `/cas/args/root`: a blob for a file, a tree for
a directory, a commit for a gitlink. A reader whose root is a gitlink gets the
commit, so it can walk history; that replaces `@git`. When nothing exists at
the path, `/cas/args/root` is absent, as an omitted optional argument already
is. Inside the tool every path is relative to `root`; `root` is the only
conversation-relative value.

A tool sees `root` and the arguments the call names, and nothing else. Other
trees it needs are `{tree}` or `{commit}` parameters, as `merge`'s `theirs`
already is.

### The result

Every tool's result is a tree in `/cas/out`:

| entry | meaning |
|---|---|
| `stdout` | REQUIRED: the text the model reads |
| `failed` | marker: the call failed. For a writer nothing is applied, whatever else is present |
| `root` | writer: the new object for the root path |
| `deleted` | writer: marker, remove whatever is at the root path |
| `commit-message` | writer, optional: the message for commits the harness makes |

A writer that has not failed has exactly one of `root` and `deleted`; neither,
or both, is an error naming the tool, so a writer that forgets its output can
never delete its root. Returning the object that was there is a no-op.

Readers move to the same shape, so `stdout` and `failed` mean one thing for
every tool. The `report` file with a `FAILED` banner, and a bare blob result,
go.

What a writer may return for `root`:

| root was | `root` may be |
|---|---|
| absent | anything |
| a blob | a blob; its mode is kept unless the tool sets one |
| a tree | a tree, which may hold gitlinks to commits the tool made |
| a commit | a commit, or a tree that the harness commits with the old commit as parent |

The harness does not check that a returned commit descends from the old one.
Squashed and force-replaced layers do not, and commits inside a returned tree
were never checked; what a writer does at its root is its job.

### Applying a writer's result

The harness puts the new object at the root path and rewrites everything
above it: each parent directory, and at each gitlink the path crosses, a new
commit of that source tree with `commit-message`, then one conversation
append.

No new machinery: diff the old root object against the new one, prefix each
change with the root path, and hand the changes to `plan_file_changes`, which
already turns changes under a gitlink into child commits, reconciles against
concurrent changes, refuses `.caos/`, and applies nothing on a conflict. That
is today's conversation-scope path, made the only one.

A side effect: a writer's cache key is its root object and arguments, so
`edit` re-keys only when its own file changes, where today a
conversation-scoped writer re-keys on every edit anywhere.

### Tags

Three remain: `@root`, `@writer`, `@request-id`. `@in`, `@git`, `@scope` and
`@call` go, and `@op` remains only for the subagent operations.

## 4. `@request-id`

`@request-id` declares an optional `request-id` parameter. When the caller
omits it, the agent harness fills in the tool call's id and `caos-cli
run-tool` a random value.

- Defaulting to the call id rather than a random value: it is just as unique,
  and it is in the transcript, so debugging, forking and replay can see which
  key a call used. A caller may pass its own value to share one result between
  calls on purpose.
- Not making it required: it would be boilerplate on every such call, and
  agents invent poor random values (`"1"`, `"abc"`), which silently brings back
  the cache hit the tag exists to prevent.

The rule: **a tool that reads or changes anything outside caos declares
`@request-id`.** Without it a second `import-source` of `main` would return the
cached old commit, a repeated publish would report a stale result, and
`ci-status` would not see CI move.

A tool that hands work to `std/github` passes its own `request-id` on, so the
github request is as fresh as the call that made it, and a retry of the outer
call with the same id reaches the same github results.

`salt` and `test-salt` stay: they re-run everything below a point, or one
tier, which a per-request id cannot.

## Migration of std tools

| tool | today | becomes |
|---|---|---|
| `read`, `ls`, `grep` | `@in` | `@root`; `file-path`/`path` relative to `root` |
| `log`, `show`, `diff` | `@in` | `@root`, a gitlink root gives the commit |
| `caos-build`, `caos-test`, `flake-update` | `@in` | `@root` |
| `write`, `edit` | `@writer @in`, conversation scope | `@writer`; `root` is the file (`file-path` goes) |
| `remove` | `@writer @in` | `@writer`; `root` is the thing removed, result `deleted` |
| `copy`, `move` | `@writer @in` | `@writer`; `root` is the nearest directory holding both paths |
| `bash-tool` | `@writer @in` | `@writer`; `root` a directory, `paths` and `cwd` relative to it |
| `move-sed`, `create-squashed-stack` | `@writer @in` | `@writer`; `root` the directory |
| `merge` | `@writer @scope source-tree` | `@writer`; `root` the gitlink, returns the merge commit |
| `github` | `@call`, one API call | `@request-id`; a batch of `api` / `push` / `import` / `head` operations, with today's `method`/`path`/`body` as the one-call form |
| git `@op` tools | `@op` | as in section 2 |
| `publish_source`, `import_source` (registered names) | built into `llm-step` | removed; the std tools replace them |

The registered file tools, `grep` and the git readers inside `llm-step` follow
the same contract or are removed in favour of their std entries (SPEC,
"CaosTools": what is registered is what has no path).

## Order of work

1. No automatic resume (section 1).
2. `@request-id`, with `std/github` moved to it.
3. `root` in the harness, accepting old and new tags during the migration.
4. The std tools, reader by reader and writer by writer.
5. `ci-status` as the first operation turned worker, since it writes nothing,
   then `publish-source`, `stack-publish`, `import-source`, `stack-sync` and
   `stack-merge-to-master`.
6. Remove `@in`, `@git`, `@scope`, `@call`, the pins and the publication
   transitions.

GIT.md, SPEC.md ("CaosTools") and std/README.md change with the steps that
change what they describe.

## Not changing

- `salt` and `test-salt`.
- The subagent and async operations. They create child conversations and
  async tasks, which are conversation records in their own right.
- The server endpoints `/git/import` and `/git/push`, and `caos import-git` /
  `caos push-git`.

## Open questions

- Whether the server accepts `/git/push` and `/git/import` from a worker, and
  under which credentials. To check before step 5.
- Whether `import-source`'s provenance should stay a `.source.json` sibling,
  which forces its root to be the whole stack directory, or move somewhere
  inside the entry it describes.
- Whether a step restart should still rejoin a pure job that is still running,
  as an optimization: it is the same content-addressed task, so this is not a
  retry, but it is also not needed for correctness.
- How `{array}` `expected` should be spelled for `stack-publish`, and whether
  `dry-run` should print it in exactly that form.
