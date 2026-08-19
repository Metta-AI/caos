# Work -- request and response

A WorkRequest is:
- an ArgTree: a git tree containing named args:
    - image: See below
    - execution-policy: a reserved, versioned description of execution behavior
      outside the image (including platform); verified by the server before cache
      lookup
    - other args, by agreement between the caller and the worker, all as git files/trees/commits
    - std (optional, but very common)
    - salt (optional): a string that is used to invalidate the cache
- stack
- trace id

A WorkResult is a git object, containing whatever the worker chose to return

WorkRequest contains ArgTree, stack, trace id, etc #todo
- The ArgTree is the cache key

We generally talk about ArgTrees, not images. An image is just one arg (see
below), so one simple ArgTree contains only an image and the reserved execution
policy; richer ArgTrees carry other args alongside them. Passing around ArgTrees
rather than images is what makes currying (below) a uniform operation.

Also note in calling that rebinding existing args is an error #todo

# Forming an ArgTree

The simplest ArgTree has no caller arguments: only its image and the reserved
execution-policy entry.

## Docker digest

image = "docker://<docker url>" (a string), using an `@sha256:<digest>`, not a
tag. The server rejects tag-based refs before consulting the result cache. The
same rule applies to the Docker base embedded in a git-tree image.

## A flake

image = git tree, containing a flake.nix and flake.lock, which are used to build the image

## Git-tree image

image = git tree with the following structure:
- base: an image ref
- overlay (optional): a git tree that will sit on top of the base
    - non-standard ownership or perms can be represented with a sidecar foo.caosmeta file for a given file foo
- env: #todo

## Currying

Currying takes an ArgTree and args and returns a new ArgTree, binding the existing args and the new args

Curry shall fail if passed an arg that is already defined in the WorkRequest

# Workers

When caos gets a WorkRequest, it builds the image specified by the ArgTree. When doing this, it adds a few pieces to the image:
- /bin/caos: the binary used by the worker to communicate with caos
- a worker user, distinct from root, so that caos can prevent the worker from tampering with content-addressable files

Add more about the contract with the worker #todo
- get/put, unreadable files, +x preservation
- /cas/out
- run-then, map-then

# Principles of reliability

Caos is reliable because:
- It dies when unexpected things happen, rather than trying to recover from errors that we didn't anticipate or don't understand
- It checks for and fixes expected issues
    - If we expect a directory, create it if necessary each time we start
    - If we expect settings on a git repo, set them each time we start
- Work is deterministic
    - The results are cached, so they need to be at least deterministic enough to satisfy the caller. For example, tests include timing info and llm results are random, but both are sufficiently deterministic for their callers

# Principles of performance

Caos is fast because:
- It caches work based on the ArgTree. The same work is never run a second time
- It takes pains to narrow trees before using them as keys, to avoid cache misses
    - For example, caos-tools/build.sh narrows the tree to just what the flake needs to build the stackbuilder image. Then it passes just the source files when running the stack-builder to build a stack
    - Compare with calculating custom keys based on a subset of the data: this causes stale values when it goes wrong
    - Compare with
- It calculates keys quickly:
    - Calculating nix paths on a cold worker took 10+ seconds. We no longer do that
    - We make git's hash code fast -- even in debug builds (the default at the moment), we are careful to keep our dependencies optimized so that hashing is fast
- It sips from git. Instead of materializing a whole tree or subtree, each step only loads what it needs
- It pushes to git only what's new

Two things need to be fast:
- Primary: Rebuild and retest everything: `time CAOS_SALT=$(date --iso=s) result/bin/caos-cli run-tool test`
    - This doesn't rebuild the stack-builder image from the flake, because that's just a function of the flake and is cached in docker
- Secondary: Build and restart on the host: `time nix build && time result/bin/caosd up`. Not part of the normal dev loop

We have various kinds of salt to control what work gets redone:
- `CAOS_SALT=$(date --iso=s)` to rerun all caos workers (but not rebuild flakes, which do not include this in their key)
- `run-tool test --test-salt=$(date --iso=s)` to rerun all tests

If these become slow:
- Sample `ps` during a run

# Secrets

**Status:** partly built. The store is carried as ephemeral run context and
resolved client-side; injection (gated by the double-check below), superset
matching over path-only readers, the entropy/`secret-hash` cache-isolation tag,
the output-scrub assertion, log masking, and the `caos secrets` entropy tooling
all exist. **Cache isolation is now complete for the eval path**: the running
worker, eval-path's `curry` returns, and — via the eval-path stripping rule —
a worker embedded through a `:@=` arg, which makes its embedder per-user too.
Builds on `.caos-expr` (eval-path, deep-deps) and map-then (server-mediated
worker starts).

**Since the ambient-`std` removal landed** (design/caos-expr.md, "Landed:
ambient `/std` is gone"), a reader is a **tree path and nothing else** — there
is no `/std/<name>` to name, so the two reader forms collapsed into one, which
is what this note always wanted. It also briefly *widened* the
caller-propagation gap: eval-path used to mark a `/std/<name>` `:@=` target, and
that was the only `:@=` marking there was. Closing it properly covers all of
`:@=` and needs no `/std` special case at all.

What remains is not about the eval path: the agent harness carries no store,
and `value:@=` is UTF-8 only. See "Remaining work".

## Problem

Some tools need secrets: the github-push tool needs an auth token, and there will be many like it. But:
- we don't want secrets in content-addressed stores, where they might leak
- we don't want secrets in keys, because we don't want to invalidate (most) keys if a secret is rotated
- we don't want secrets in one worker/arg tree to be able to be read from it by another worker

## Solution

`.caos-secrets`:
- Secrets live in a git-ignored .caos-secrets directory
- Each secret file contains the secret's value and a list of workers that can read the secret. This is formatted as a repeated-key file. For example:
```
# Optional name. Defalts to the name of the file. This is the name that is used in the worker for /secret/<name>
name=<name>
entropy=...
# Inline secret
value=<secret key>
# External key
value:@=<file containing key>
# A reader is a PATH to an expression, without arguments. It is eval-path'd to
# an arg tree
reader=std/github-push
reader=tools/deploy
```
- When a call stack is started, such as `caos-cli run`, we read the current source tree and the list of secrets. Readers in secrets are matched against the tree. Any worker named as a reader is granted access to the secret. These workers have a hash of the names and entropy of all exposed secrets injected into them as /cas/args/secret-hash
- Something is considered to be the same worker (ie, to have access to the secret) if it its arg tree is a superset of the reader's arg tree and secret-hash matches the set of secrets that the server computes for it
- Each granted secret contributes its (worker-visible name, entropy) to a
  `secret-hash` entry folded into the worker's arg tree (visible at
  `/cas/args/secret-hash`). This makes two users with different secrets see
  different cache keys — but keps the secret's *value* out (so rotating a value
  doesn't bust the cache), and stores the *digest* of the entropy, never the
  entropy itself (the entropy is a bearer capability for the cache: knowing it
  reconstructs the key of any run that used it). The name is included because a
  different mount name would make the worker run differently
- `secret-hash` in the arg tree also means that the cache key of a worker will depend on the secrets exposed to things that it calls, which is required to avoid accidentally sharing data derived from secrets through the cache

Worker experience:
- If a secret is visible to a worker, it is injected into a worker in `/secret/<name>`
- We attempt to scrub secret values from the logs of workers
- We attempt to check files that are added to git with `caos put` for secret values. Any new file (hash not in git) that contains the value of a secret that is visible to this worker is rejected

Correctness requirements: a run's *identity* (name + entropy of each granted
secret) is in the cache key via `secret-hash`, but the secret's **value** is
not. So:
- A worker must fail if the secret is missing or invalid.
- A result may depend on *which* secret it was granted (name + entropy) — that
  is isolated per-user by `secret-hash` — but must not depend on the value's
  *bytes* beyond what rotating the **entropy** would refresh. Rotate the entropy
  when you rotate a value the result genuinely depends on; a plain value
  rotation (e.g. a token for the same account fetching the same content) keeps
  the cache, which is the point.
- Concretely: a worker may fail on an invalid secret, but must not return, say,
  a listing filtered to what one value's account can see, unless that value's
  identity is pinned by the entropy.

Server behavior:
- The server passes the list of secrets and the tree against which to evaluate them from one work request to the next, along with the stack
- When dispatching a work request, the server injects a secret into the worker only if **both**: (a) the worker's arg tree is a superset of one of the secret's readers (identity), **and** (b) the worker's arg tree already carries a `secret-hash` entry equal to the one the server computes for the granted set. Condition (b) proves the worker was produced by eval with this store — so a secret's value can only ever reach a worker whose cache key *already* reflects that secret. A reader-match without the matching `secret-hash` (a worker not built through eval, or a stale/forged tree) is refused, fail-closed. This ties injection to isolation: injection ⟹ the isolating hash is in the key.

Note that this means that the server sees all secrets. We can revisit if this becomes a problem

## Remaining work

- **The agent harness carries no store.** `caos talk`/`chat` (`chat.rs`'s
  `turn` and `generate_conversation_title`) pass an empty store and an empty
  header, so an agent turn — and every tool it invokes as a sub-run — is granted
  nothing. This is where the note's own motivating example lives (an agent
  reaching for github-push), so it is a hole, not a boundary. It was never
  decided: the `&[]` is what the parameter-threading left behind. Filling it is
  one call (`build_secret_store` before `prepare_request`, its header on
  `request_compute`), but it is a **policy** choice first: a store-carrying turn
  is per-user keyed via `secret-hash`, so every chat that matches a reader stops
  sharing cache with other users. Worth deciding explicitly rather than by
  default.

- **Binary `value:@=`.** Read but kept UTF-8 (binary/multiline later).

- **`run`-form `.caos-expr` grants** are deliberately unresolved (a grant must
  never trigger compute); likely permanent.

- **Shared-server exposure.** Carrying the whole store means a shared server
  sees values it never injects (sub-runs aren't known ahead of time, so the
  client can't pre-filter to the granted subset). Moot for a per-user/local
  server; a tighter hand-off is future work.

# Codebase

## Before committing

Run `time result/bin/caos-cli run-tool test`

## General
- A root-level flake.nix and flake.lock SHALL install all of the dependencies needed to build the code
- For now, the code is written in Rust. This was probably a mistake:
    - Rust can fetch dependencies just based on a cargo file, but it won't compile them without top-level sources. This is the difference between roughly 3 seconds and 12 seconds to compile cold. Go doesn't have this problem
    - Rust compiles somewhat slowly compared to Go
    - Rust's toolchain is much larger than Go's, and includes less, so we need to fetch even more
- For now, we write scripts in Bash. This was probably a mistake:
    - Humans (Malcolm) definitely have trouble reading and writing bash
    - But the bots do too! They've started keeping notes on sharp edges with Bash in AGENTS.md
    - Go supports `go run`, which compiles and runs a .go file in one shot
- In the future, The code will be written in go, including scripts, which SHALL be run by `go run`
- Comments should focus on decisions that a naive reader might undo instead of narrating the journey that a bot took to arrive at a solution

## Worker scripts

- Workers shall be written in a single file that covers all the stages of the worker. That is, when a worker calls map-then or run-then, the next next stage should be in the same file. The worker shall use a `stage` argument to track which stage it is up to

# Server

- Has a local git repo
    - GC is disabled
- shall listen on port 80 and respond to requests:
    - Git push/pull requests are routed to git to handle against the repo
    - WorkRequests as described below. The input is the hash of the ArgTree as a git tree (the stack and trace id travel alongside it, not inside the hashed tree). The result is the hash of the WorkResult

# Misc

- `run-tool` does not fetch the output of the tool that it runs. It just prints the hash and the stdout part
- `caos put` checks whether the server has each object while descending the tree, to avoid putting things that are already there
- `refs/caos/bins` does NOT exist. Nothing creates it and nothing reads it: a
  tool gets the tree under test and builds from source, so there is no ref of
  prebuilt host binaries to resolve. Do not reintroduce one.

# From agents

Agents, add more notes here

# Tools

A tool is a worker script at `caos-tools/<name>.sh`, run as an ordinary caos
job over the workspace tree. It has TWO callers and one contract: an agent's
tool call (`worker-llm-step`), and `caos-cli run-tool <name> [--k=v ...]` by
hand. Both build the same ArgTree, so a tool cannot behave differently
depending on who invoked it.

## Invocation

- The job is `curry(<tools image>, worker1=<the script>, <declared args>)` run
  with the workspace tree as `--in`
- Tools are discovered fresh from the CURRENT workspace on every LLM round and
  resolved again at INVOCATION time, so an agent that edits a tool sees the
  change on its next call, within the same turn
- `bash`, `grep`, `read`, `ls`, `write` and `edit` are reserved. A
  `caos-tools/bash.sh` is ignored, not registered — the model's primitives,
  including the repair path for a broken tool edit, stay stable whatever the
  tree carries
- Subdirectories of `caos-tools/` are helpers, not tools

## Declaring a tool

Marker lines in the script's header comment:

- `#@doc <text>` — one or more lines, joined, become the tool's description.
  A tool with none gets a placeholder
- `#@arg <name> <description>` — a REQUIRED parameter
- `#@arg [<name>] <description>` — an OPTIONAL parameter

Arg names are `[a-z][a-z0-9-]*`. `in`, `worker1`, `base`, `execution-policy`,
`std` and `salt` are refused: the interpreter binds those itself and currying
SHALL fail on a rebind. A malformed `#@arg` line is skipped with a message,
never silently turned into an arg the model cannot use.

Every parameter is declared to the model as a string, because every arg
reaches the script as a blob whatever JSON type it left the model as. A tool
with no `#@arg` lines takes no parameters: the workspace tree IS its input.

## Receiving args

- A bound arg lands at `/cas/args/<name>`, a lazy placeholder like any other
  arg — `caos get` it before reading
- An omitted optional arg simply does not exist; test with `[ -e ]`
- Values are never shell-interpolated. They are argv elements to `caos curry`,
  then bytes in a file
- Args are part of the ArgTree, and the ArgTree is the cache key. The same
  tool called with different args is a different job; a repeat of either is a
  cache hit. Tools need no keying logic of their own

## Returning a result

The result is a git object whose shape the tool chooses. Three conventions,
applied identically by `run-tool` and by the agent harness:

- a BLOB — printed verbatim. The shape for a tool whose answer is text
- a tree with a `report` file — the report is printed, and a `FAILED` banner
  in it marks the call a failure. Do NOT use this shape for a tool that
  returns arbitrary logs, which say `FAILED` all the time
- any other tree — its top-level listing is shown

Long results are truncated by keeping the TAIL, so a tool SHALL put its
summary and its diagnostics last.

A tool's printed answer SHALL be the same for both callers. A convention that
shows the human more than the agent — as an extra pass over the result tree
once did — makes the tool untestable through the surface an agent uses. If a
result needs more detail than its report carries, expose the detail as ANOTHER
TOOL taking a hash (`test` and `test-result`), not as richer printing.

## Failure

- A caller's mistake — a missing required arg, an undeclared one, a
  non-scalar value — is answered as an error tool_result the model can read
  and correct. The sub-run never launches
- A tool's own EXPECTED failures SHALL be values too, not job errors: a tool
  is often called precisely because something already went wrong, and a job
  error there takes the agent's turn down with it
- Unexpected failures die, per the reliability principles above

# Merging and conflict resolution

An agent resolves a git merge from inside a conversation. The obstacle is
that git's "resolve in the working tree, then `git add` to the index, then
`git commit`" ceremony has no place to live: there is no index, and a
conversation advances by whole commits, not staged files. Caos collapses the
ceremony — resolving a conflict is just producing the next commit — and
provides two tools:

- `merge --theirs=<ref|hash>` — three-way merge the given commit into the
  conversation head. A git-bearing SUB-RUN, not in-process (below).

`read`/`ls` default to the current workspace but accept a `root` — a commit,
tree, or blob hash — to read/list as of another revision (below). A bare blob
`root` with no path reads that object directly (what a standalone `read-oid`
once did).

## Tools thread a commit, not a tree

To let `merge` record `theirs` as an ancestor, a tool's unit of work is a
COMMIT, not a bare tree: the step loop threads a workspace commit through the
call queue, and every tool is `commit -> (commit, result)`.

- **Read-only tools** (`read`, `ls`, `grep`) return the input
  commit UNCHANGED — no new object, no no-op commit.
- **Mutations** (`write`, `edit`, `bash`, tree tools) return a single-parent
  commit `commit(new tree, parent = input commit)`.
- **`merge`** returns a two-parent commit `commit(merged tree, parents =
  [input commit, theirs])` — the only tool that fills a second parent, but
  otherwise an ordinary tool with the ordinary signature.

Reachability does the rest. A merge commit `M` carries both its parents, so
anyone who has `M` has `ours` and `theirs`; the sole requirement is that `M`
be reachable from the conversation head. That is automatic: the step commit a
round mints hangs the round's FINAL workspace commit off itself (as a second
parent — the first-parent spine and the transcript walk are untouched), and
that workspace commit chains back through the round's per-mutation commits —
`M` among them — to the head. So `theirs` is wired in with no note, no
sidecar, no turn-ending special case; `merge` is not privileged, it just
returns the commit it built.

This makes commits per MUTATION (reads stay free). Fine under the squash
workflow below; caching is unaffected — sub-runs key on their input TREE, not
on commits.

## `merge --theirs=<commit>`

- Takes exactly one commit arg (`theirs`). The other side (`ours`) is the
  workspace commit threaded into the call — the head plus whatever this turn
  already did, so no earlier edit is cut out.
- The merge is index-free and worktree-free: `git merge-tree --write-tree
  <ours> <theirs>` is a pure `(commit, commit) -> (tree, conflict report)`,
  which memoizes like any other job and needs no materialized working copy
  (the harness forbids one). The merge base is `merge-base(ours, theirs)`,
  which `merge-tree` derives from the commit graph.
- It runs the real `git` binary in its own worker. `.caos/conflicts`
  and the inline markers are straight from git's own output (below), so we want
  `merge-tree`'s exact notation, not a reimplementation. gix is a dependency
  but carries no merge (`gix-merge` is not pulled in), and its output would not
  match git's notation anyway. So `merge` is a decomposed compute tool like
  `bash`/`build`/`test`; the file tools (`read`/`ls`/`write`/`edit`) are in-process.
- Its image is a small git worker — a `std/merge` flake
  (`nixpkgs.gitMinimal`) run as `curry(std/runner, worker1=<merge script>)`,
  the same flake-image pattern as `std/bash`. Not `std/cargo` (which has git
  but is a heavy image and the wrong home) and not folded into the bash-tool
  image (whose surface stays minimal). The script reconstructs a git odb from
  the `ours`/`theirs` commit closures in `/cas` (a merge is inherently a
  both-whole-trees op — the one place laziness can't help), runs `merge-tree`,
  writes `.caos/conflicts`, and `put-commit`s the two-parent commit as its
  result.
- Clean merge → `M`'s tree is the merged workspace and the merge is done.
- Conflicts → `M`'s tree carries inline conflict markers in the text files
  (what the agent edits), plus a reserved `.caos/conflicts` file (below). The
  agent resolves over subsequent turns; each resolution is an ordinary
  mutation commit on top of `M`.

## Resolving `--theirs` (the ref snapshot)

The model says "merge in `main`", but a ref name only exists in the user's git
repo — the merge worker, mid-turn on the compute network, has no refs, and the
model doesn't know hashes. So `--theirs` is resolved on the CLIENT, at turn
START — the only place the refs live and the only moment the client is in the
loop (a tool call three rounds deep cannot reach back into the repo):

- The client resolves a small, curated set of refs to hashes — `HEAD`'s
  upstream, `main`/`master`, the `origin` default — `ensure_pushed`es their
  closures (onto the CONTENT-ADDRESSED `refs/caos/req/<hash>`, exactly as
  `--head:commit` is pushed; NO semantic ref like `main` is ever written to the
  shared server, so users never contend for a name), and curries a
  name→hash MAP into the llm-step worker as an ordinary blob arg.
- The `merge` tool resolves `--theirs` against that map: a known ref name → its
  snapshotted hash; a bare hash → used directly; anything else → an is_error
  tool_result listing the available names. `ours` is never named — it is the
  threaded workspace commit.

**Snapshot semantics**, deliberately: "merge in `main`" merges `main` as it was
when the turn started, so the merge is deterministic and immune to `main`
moving mid-turn. `ensure_pushed` negotiates against the server, so an unmoved
`main` re-pushes nothing — the steady-state cost is the delta since last time.

## `.caos/conflicts`

The authoritative set of unresolved conflicts, produced by the merge itself —
not re-derived by grepping for markers. Grepping is rejected as the store: it
false-negatives on the conflicts that have no textual marker at all
(modify/delete, binary, type/mode change), which it would silently report as
resolved.

It holds git's own unmerged notation verbatim — `git ls-files -u` rows,
`<mode> <oid> <stage> <path>` (stage 1 = base, 2 = ours, 3 = theirs) — plus
git's informational-messages block as human hints. That notation is strictly
richer than text markers:

- content conflict → the stage rows' oids differ (and markers are also
  written into the file, for editing);
- perm/type conflict → the stage rows' MODES differ (regular ↔ symlink ↔
  gitlink); nothing lives in the bytes, so there is no marker — resolving
  means choosing the entry's mode;
- modify/delete → a stage is simply ABSENT.

The agent resolves a path by editing the file (removing markers) or fixing
the entry, then DELETING that path's rows from `.caos/conflicts`. That
deletion IS the per-path `git add` — an explicit "this one's done"
assertion, trusted exactly as git trusts `add` (no re-scan). An empty
`.caos/conflicts` means done; the agent need not remove the file (inline tools
have no delete — `bash rm` does, for a clean mid-conversation checkout).

`.caos/conflicts` is workspace state: it lives in the workspace tree `ws` the
tools thread, so it rides into `M` and every mutation commit on top of it for
free — it is part of `ws` like any file. This does NOT collide with
`.caos/step.json`, which shares the `.caos/` name but never the same place:
step.json exists ONLY in a step-commit tree (`mint_step` injects it), never in
`ws`; conflicts exists ONLY in `ws`. They meet in one `.caos/` directory only
inside a step tree, and the harness tells them apart by FILENAME. So four small
local rules, no "persistence exemption":

- **Inline tools** refuse `.caos/step.json` specifically, not all of `.caos/`,
  so `.caos/conflicts` is editable like any file (deleting a path's rows is an
  `edit`).
- **`mint_step`** PRESERVES an existing `.caos/` when it injects `step.json`
  (symlinking `.caos/conflicts` in alongside), rather than assuming `.caos/` is
  absent.
- **Compute tools** (`bash`/build/test) see `ws` as-is, `.caos/conflicts`
  included — it is workspace state. (A build run mid-merge keys on a tree that
  still carries the file, so it won't cache-hit the post-resolution build;
  negligible, and only during resolution.)
- **Publish** (the tui's PR flow) strips `.caos/` from the snapshot AFTER the
  guard below, so scaffolding never reaches the PR — a leftover or empty
  `.caos/conflicts` cannot leak.

Both `.caos/conflicts` and the inline markers sit in the diff the whole time,
so a mid-merge head is fully reviewable.

## Reading by hash (`read`/`ls` with `root`)

Both file readers default to the current workspace but accept a `root` — a
commit, tree, or blob hash — to read/list as of another revision. A commit or
tree `root` navigates its tree by path; a bare blob `root` (no path) reads that
object directly. This is load-bearing, not a convenience: the stage oids in
`.caos/conflicts` name content that is NOT reachable through any workspace path
— the base (stage 1), and either side of a modify/delete, binary, or type
conflict, none of which appear at the path. Without a by-hash read the agent
cannot see what it is choosing between. It began as a standalone `read-oid`
blob reader; folding it into `read`'s `root` generalized the same fetch to any
revision (and made the history tools' hashes readable the same way).

## Guards and workflow

- NO empty-`.caos/conflicts` guard on turn completion. Asking the user how to
  resolve a conflict IS ending a turn mid-merge; a turn-completion guard would
  make that impossible.
- Correctness of a resolution is checked by BUILD/TEST at the end of the
  resolution turn (a leftover marker does not compile) — not by a marker
  re-scan, which cannot tell a real marker from a bad resolution.
- The one place to refuse or loudly warn on a non-empty `.caos/conflicts` or a
  remaining marker is PUBLISH (the tui's PR flow) — the moment work actually
  leaves the conversation.

## Caveat

Per-mutation commits mean real, sometimes marker-bearing, non-building commits
land in history. These conversations produce throwaway histories that are only
usable once SQUASHED, so this is fine — but an intermediate commit (a
mid-resolution merge commit especially) is NOT safe to cherry-pick or check out
in isolation.
