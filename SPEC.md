# Work -- request and response

A WorkRequest is:
- an ArgTree: a git tree containing named args:
    - image: See below
    - other args, by agreement between the caller and the worker, all as git files/trees/commits
    - std (optional, but very common)
    - salt (optional): a string that is used to invalidate the cache
- stack
- trace id

A WorkResult is a git object, containing whatever the worker chose to return

WorkRequest contains ArgTree, stack, trace id, etc #todo
- The ArgTree is the cache key

We generally talk about ArgTrees, not images. An image is just one arg (see
below), so one simple ArgTree is one that only contains an image; richer
ArgTrees carry other args alongside it. Passing around ArgTrees rather than
images is what makes currying (below) a uniform operation.

Also note in calling that rebinding existing args is an error #todo

# Forming an ArgTree

The simplest ArgTree is one that only specifies an image

## Docker digest

image = "docker://<docker url>" (a string), using a hash/digest, not a tag

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
    - WorkRequests as described below. The input is the hash of the WorkRequest as a git tree. The result is the hash of the WorkResult

# Misc

- `run-tool` does not fetch the output of the tool that it runs. It just prints the hash and the stdout part
- `caos put` checks whether the server has each object while descending the tree, to avoid putting things that are already there
- Mentions of `refs/caos/bins` are inconsistent. Does it still exist?

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

Arg names are `[a-z][a-z0-9-]*`. `in`, `worker1`, `image`, `std` and `salt`
are refused: the interpreter binds those itself and currying SHALL fail on a
rebind. A malformed `#@arg` line is skipped with a message, never silently
turned into an arg the model cannot use.

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
- `read-oid <oid>` — read a blob by hash. In-process at the hash level, like
  `read`/`ls`/`write`/`edit` (below).

## Tools thread a commit, not a tree

To let `merge` record `theirs` as an ancestor, a tool's unit of work is a
COMMIT, not a bare tree: the step loop threads a workspace commit through the
call queue, and every tool is `commit -> (commit, result)`.

- **Read-only tools** (`read`, `ls`, `grep`, `read-oid`) return the input
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
  `bash`/`build`/`test`, and only `read-oid` is in-process.
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

## `read-oid`

Bounded blob read by hash — same contract as `read` (100KB / offset+limit).
It is load-bearing, not a convenience: the stage oids in `.caos/conflicts`
name content that is NOT reachable through any workspace path — the base
(stage 1), and either side of a modify/delete, binary, or type conflict, none
of which appear at the path. Without `read-oid` the agent cannot see what it
is choosing between. (Tree-oid listing is a cheap optional companion.)

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
