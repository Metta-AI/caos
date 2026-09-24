# Work -- request and response

A WorkRequest is:
- an ArgTree: a git tree containing named args:
    - image: See below
    - other args, by agreement between the caller and the worker, all as git files/trees/commits
    - salt (optional): a string that is used to invalidate the cache
- stack

A WorkResult is a git object, containing whatever the worker chose to return

WorkRequest contains ArgTree, stack, etc #todo
- The ArgTree is the cache key

We generally talk about ArgTrees, not images. An image is just one arg (see
below), so one simple ArgTree is one that only contains an image; richer
ArgTrees carry other args alongside it. Passing around ArgTrees rather than
images is what makes currying (below) a uniform operation.

Also note in calling that rebinding existing args is an error #todo

# Forming an ArgTree

The simplest ArgTree is one that only specifies an image

## Docker digest

image = "docker://<docker url>" (a string), using an `@sha256:<digest>`, not a
tag. The server rejects tag-based refs before pulling or running them. The same
rule applies to the Docker base embedded in a git-tree image.

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
    - For example, std/caos-build narrows the tree to just what the flake needs to build the stackbuilder image. Then it passes just the source files when running the stack-builder to build a stack
    - Compare with calculating custom keys based on a subset of the data: this causes stale values when it goes wrong
    - Compare with
- It calculates keys quickly:
    - Calculating nix paths on a cold worker took 10+ seconds. We no longer do that
    - We make git's hash code fast -- even in debug builds (the default at the moment), we are careful to keep our dependencies optimized so that hashing is fast
- It sips from git. Instead of materializing a whole tree or subtree, each step only loads what it needs
- It pushes to git only what's new

Two things need to be fast:
- Primary: Rebuild and retest everything: `time result/bin/caos-cli run-tool caos-test --test-salt=$(date --iso=s)`
    - This doesn't rebuild the stack-builder image from the flake, because that's just a function of the flake and is cached in docker
- Secondary: Build and restart on the host: `time nix build && time result/bin/caosd up`. Not part of the normal dev loop

We have various kinds of salt to control what work gets redone:
- `CAOS_SALT=$(date --iso=s)` to rerun all caos workers (but not rebuild flakes, which do not include this in their key)
- `run-tool caos-test --test-salt=$(date --iso=s)` to rerun all tests

If these become slow:
- Sample `ps` during a run

# CaosTools

**Status:** built, in the minimal form below. The "Not built" list at the end of
this section is the rest, and none of it is assumed anywhere.

A CaosTool is an ArgTree designed to be run by an agent, distinguished from any
other ArgTree by carrying a `help` argument that describes the arguments it
accepts. A tool is therefore any directory in the conversation whose
`.caos-expr` binds a `--help`. There is no registry, no reserved directory name,
and nothing enumerates them.

Two built-in tools address one by PATH:

- `tool_help --path=<conversation path>` returns that tool's description and
  parameters. It evaluates the path, including the target expression, and reads
  the resulting ArgTree's `help` binding. This can build the tool; it does not
  invoke it. Missing paths and invalid tools are recoverable tool errors.
  Missing paths also name sibling directories.
- `run_tool --path=<conversation path> --arguments=<object>` evaluates the same
  path, validates arguments against the evaluated help, and invokes the tool.
  An undeclared argument, a missing required one, or a non-string value is
  answered as an error tool_result before invocation.

Additionally a human runs a tool with `caos-cli run-tool <path> [--k=v ...]`.

**Agents discover tools from documentation** — a repository's own `AGENTS.md`,
README or design docs — not from a listing. That is the whole point of the
change: enumerating them meant injecting every source tree's tools into the
system prompt, so a conversation that gained a source tree mid-run re-keyed the
prompt and grew it per tree. Naming a path instead makes the tool list
TREE-INDEPENDENT, which is also what lets `caos mcp`'s listing be cached and
makes a client that ignores `tools/list_changed` correct rather than stale. The
price is real and deliberate: an undocumented tool is invisible. Prose may drift
from the tool; `tool_help` is the authority.

## Resolution

A path names a tree and a tool within it: the conversation-path prefix selects
the source tree, and the remainder is evaluated against THAT TREE'S ROOT. Both
halves matter. Evaluating against the conversation root instead would skip a
repository's root `.caos-expr`, so `--base:@=DEEP-DEPS/<x>` inside the tool
would resolve to nothing. And it is why a human's `run-tool` lands on the same
ArgTree: the human has one source tree — the worktree — so the prefix is empty
and the remainder is the whole path.

Both tools use ordinary CAOS evaluation, including its dependency-resolution
semantics. Thus a root expression may generate a `tools/check` directory absent
from the stored tree: both tools can reach and evaluate it.

Resolution uses the current conversation snapshot on every call. The
definition's evaluated tree is separate from the input: what invocation binds as
`in` is the original selected source tree — or the original conversation tree
when no source-tree prefix was selected — and it binds it ONLY for a tool that
declared `@in`.

MCP resolves the requested `path` against that same snapshot on the client,
where pinned `:@@=` dependencies can be fetched. It hands the evaluated tool
to both callers. The handoff names its input tree and path, so a changed snapshot
cannot consume a stale result. Workers continue to use evaluation continuations
and do not fetch locators themselves.

## Help text

The `help` string is a JAVADOC comment, and it is authored as a HERE-STRING in
the expression itself (design/caos-expr.md):

```
HELP=<<END
Print one test's complete record from a `test` run.
@param hash The hash the `test` report prints beside a test's name.
END
curry --base:@=DEEP-DEPS/bash --worker1:@=worker.sh --help=$HELP
```

Authoring it there rather than in the script is what SPLITS the two identities:
a `.caos-expr` is stripped from the tree its own expression is evaluated
against, so editing the docs re-keys the tool's ArgTree — the thing a caller
runs — and re-keys NOTHING the tool builds from.

The free text before the first block tag is the tool's description (a tool
with none gets a placeholder); `@param` tags declare the parameters:
- `@param <name> <description>` — a REQUIRED parameter
- `@param [<name>] <description>` — an OPTIONAL parameter

The bracketed name is the one extension over stock javadoc, which has no
notion of an optional parameter.

Three bare tags are flags:
- `@writer` — this tool PROPOSES A CHANGE to the tree it is run on, rather
  than returning a value. Absent means READ-ONLY, which is the default because
  it is the safe reading of a tool that forgot to say
- `@in` — bind the tree it is run on as `in`. Absent means DO NOT, and that is
  the point: see "Receiving args"
- `@git` — bind the source tree commit and the turn's ref snapshot

The tags live in the help rather than as their own args so that `tool_help` can
report it WITHOUT EVALUATING: "will this change my tree?" is the second most
important fact about a tool, so the description has to carry it, and an arg
would be readable only by building the tool. It also costs no reserved name.

**A tag this parser cannot act on is reported, never absorbed.** A misspelled
`@writer` would otherwise leave a writer read-only — and a read-only tool's
proposal is discarded as a value, so the edit simply does not happen, with no
error anywhere. `@params x` is likewise not `@param` with `s x` after it; it
once minted an arg named `s` and lost the real one.

Arg names are `[a-z][a-z0-9-]*`. `in`, `worker1`, `base`, `salt`, `wc`, `refs`
and `help` are refused: the interpreter, or the tool's own expression, binds
those itself and currying SHALL fail on a rebind. A malformed `@param` tag is
skipped with a message, never silently turned into an arg the model cannot use.

A parameter is a STRING unless it says otherwise, because every arg reaches the
script as a blob whatever JSON type it left the model as. `@param {<type>}
<name>` declares another: `{string}` is the default said out loud, and
`{array}` is a list of strings, which the model sends as a JSON array and which
is CURRIED AS ONE NEWLINE-SEPARATED BLOB — all an arg can be by the time a
script reads it. `{tree}` and `{commit}` bind the OBJECT rather than
bytes naming it, so the script finds a real tree or commit at
`/cas/args/<name>`. An UNKNOWN type fails the whole tag rather than falling back
to a string, because silently narrowing an array parameter takes a capability
away with nothing to notice.

An object parameter is still a STRING to the model — it NAMES one:

- `{tree}` takes a CONVERSATION-RELATIVE PATH or a tree hash. The path resolves
  against the conversation tree, not the source tree the tool runs in; that is
  the vocabulary the model has, and the only one that can name a tree OTHER
  than the one the tool is running over
- `{commit}` takes a hash, and only a hash. A path cannot name a commit because
  `caos resolve` traverses a gitlink to the tree inside it, so a path is
  refused with that explanation rather than quietly yielding a tree
- `:@@=` is never offered to an agent: a locator is resolved by the CLIENT
  only, and an agent's evaluation runs server-side

The naming rule rides in the parameter's DESCRIPTION, since that is all the
model sees. Resolution happens when the call is validated, and what is carried
onward is an OID: the curry happens in a later worker invocation than the
validation, and a `/cas` path from the earlier one means nothing in the later.
A commit then binds as `:@=` on a freshly materialized path, which preserves
its kind — the way `merge` binds `ours`, and the reason no new arg type is
needed for one. `tool_help` states each one's name,
whether it is required, and its documentation as prose rather than as a JSON
schema — the model reads it, and a schema dump is the registry that was just
removed. A tool with no `@param` tags takes no parameters, and one that
declares no `@in` either has no input beyond its own expression.

## Invocation

- The job is `curry(<tool arg tree>, <declared args>)`, where
  `<tool arg tree>` is what evaluating the tool's path yields — run over the
  tree as `--in` only if the tool declared `@in`
- Evaluation happens where blocking is legal. A worker may not block, so the
  agent's harness tail-calls `eval-path-then` and curries in the callback
  (design/map-then.md); `caos-cli run-tool` evaluates directly. Both land on
  the same ArgTree, so a hand-run and an agent call are one cache entry
- Args are part of the ArgTree, and the ArgTree is the cache key. The same
  tool called with different args is a different job; a repeat of either is a
  cache hit. Tools need no keying logic of their own

## Receiving args

- A bound arg lands at `/cas/args/<name>`, a lazy placeholder like any other
  arg — `caos get` it before reading. **`/cas/args/in`, not `/cas/in`**
- An omitted optional arg simply does not exist; test with `[ -e ]`
- Values are never shell-interpolated. They are argv elements to `caos curry`,
  then bytes in a file
- An `{array}` parameter arrives as one blob of NEWLINE-SEPARATED elements

**`in` is bound only for a tool that declared `@in`**, and the reason is the
cache key. A tree is the biggest thing that can enter an ArgTree, so binding
one a tool never reads makes it re-key on every edit to a tree it ignores:
`caos-test-result`, whose entire input is a hash, could not hit the memo twice
in a row. `grep` has always done this right — it binds only the scope it
searches, which is why a scoped grep is cheap — and every reader now follows it.

A tool whose `in` is something it BUILDS rather than the tree it was run on
declares no `@in`: `bash`'s is a `{tree, cmd, cwd, paths}` envelope and `grep`'s
is the scope, and each is bound by its own dispatch.

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
TOOL taking a hash (`caos-test` and `caos-test-result`), not as richer printing.

## Writers

A tool that declares `@writer` PROPOSES A CHANGE to the tree it was run on
instead of returning a value. Its result is `{prop, out, message?, failed?}`:

- `prop` — the proposal. A TREE, and the harness mints the commit, which is
  what lets almost every writer never handle a commit at all; or a COMMIT, for
  the one case a second parent justifies (`merge`). A returned commit MUST
  descend from the commit the tool was given: `reconcile` errors rather than
  conflicts otherwise, so the harness checks it first and answers with an
  error tool_result naming the tool and both commits
- `out` — the text the model reads. REQUIRED: substituting a generic line for
  a tool that forgot it would hide the tool's silence behind the harness's
  voice
- `message` — an optional commit message, used when the harness mints. Without
  it the mint falls back to the tool's path
- `failed` — present means the call failed. A marker entry rather than a banner
  in `out`, because `out` is arbitrary logs and a build that printed `FAILED`
  in passing would condemn a good proposal

**NO TOOL'S WRITER-NESS DEPENDS ON ITS NAME.** Every writer declares itself,
built-ins included: `bash` and `merge` carry `@writer` in their own
`.caos-expr`, and the harness reads it there. What remains keyed by name in
`callback_result` is `grep`'s renderer for its sparse match tree and the
subagent join — presentation and plumbing, not the right to change a tree.

**SCOPE comes from the invocation, not the declaration.** A writer run on a
source tree returns a source commit; one run on the conversation returns a
conversation commit. The path the caller named already decides which, so the
declaration says only WHETHER a tool writes, never where.

A writer's `out` SHALL be its own. Composing it in the harness — as the merge
conflict report once was — makes the tool's answer depend on who ran it, which
is the same reason the `help` lives in the expression.

## Failure

- A caller's mistake — a path that is not a tool, a missing required arg, an
  undeclared one, a non-scalar value — is answered as an error tool_result the
  model can read and correct. The tool's own run never launches (the path
  evaluation may still have happened; it is not the tool)
- A tool's own EXPECTED failures SHALL be values too, not job errors: a tool
  is often called precisely because something already went wrong, and a job
  error there takes the agent's turn down with it
- Unexpected failures die, per the reliability principles above

## Not built

Everything here was considered and deliberately deferred. Nothing above depends
on any of it, and each is written down because the reason is easy to lose.

- **Describing a tool evaluates it.** `tool_help` reads help from the evaluated
  ArgTree, so describing a compiled tool may build it. Skipping the target
  expression is a possible performance optimization when help is in its text.
- **The std tools (`caos-build`, `caos-test`, `caos-test-result`) are addressed
  by NAME, and that is now a CHOICE rather than a gap.** Path-addressing them
  was planned and dropped: `--base:@=std/caos-test` evaluates against the
  selected source tree's root, and an ordinary repository has no `std/`, so it
  would work for caos' own tree and for one that mounts caos' std through a
  `:@@=` locator and break `caos-test` everywhere else — while also losing the
  guarantee that a harness-provided tool is always offered. The reason
  path-addressing was right for REPOSITORY tools does not apply here either:
  enumeration scaled badly because the set grew and changed per source tree,
  and these are a fixed three whose declarations never move. They already read
  their help from their images like `bash` and `merge` do, so nothing is
  inconsistent. Revisit only if a repository needs to REPLACE one.
- **`caos-cli run-tool` and `caos-cli run` are still separate verbs**, and
  `run-tool` does not validate against the help, so "both callers build the same
  ArgTree" is a goal rather than an invariant.
- **Locator resolution requires a client.** MCP pre-resolves the requested
  tool path, including pinned ancestor dependencies. A turn driven entirely
  by workers still cannot resolve a new `:@@=` locator; its dependencies must
  already be available as content. There is no worker-side remote fetch.

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

The agent harness carries the same store: conversation preparation resolves
`llm-step` with it, the admitted request includes the resulting isolation
identity, and foreground or recovery dispatches send the store out of band.
Both conversation LLM workers read `anthropic-api-key` from `/secret`, never
from a curried arg. `value:@=` remains UTF-8 only; see "Remaining work".

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
# External key. Relative paths resolve from this secret file's directory.
value:@=<file containing key>
# A reader is a PATH to an expression, without arguments. It is eval-path'd to
# an arg tree
reader=std/github-push
reader=tools/deploy
# Use `value:env=...` and `entropy:env=...` to pull from the environment. If value comes from the env, entropy must too
```
- When a call stack is started, such as `caos-cli run`, we read the current source tree and the list of secrets. Readers in secrets are matched against the tree. Any worker named as a reader is granted access to the secret. These workers have a hash of the names and entropy of all exposed secrets injected into them as /cas/args/secret-hash
- A reader naming a path the tree does not carry is ignored, with a warning
  on stderr. Otherwise, it's too easy for a bad edit to prevent the tui from running
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

- **Binary `value:@=`.** Read but kept UTF-8 (binary/multiline later).

- **`run`-form `.caos-expr` grants** are deliberately unresolved (a grant must
  never trigger compute); likely permanent.

- **Shared-server exposure.** Carrying the whole store means a shared server
  sees values it never injects (sub-runs aren't known ahead of time, so the
  client can't pre-filter to the granted subset). Moot for a per-user/local
  server; a tighter hand-off is future work.

# Agent/harness integration

When a user uses caos with an agent harness, caos creates a conversation commit based on the repo that it's started from. This repo should be some version of a caos-client repo, not a normal repo that contains the files that the user wants to work on. The repo:
- Must contain flake.nix and flake.lock with a caos input, and a matching .caos-expr that loads the caos flake
- Possibly contains a some readmes and agents.md
- Possibly contains some caos expressions that load in other repos, that
contain helpful tools, and the readmes describe those tools 

When starting a session:
- The claude cloud env's setup script reads the flake/expression's caos url and runs `<url>/integrations/claude-code/cloud/install.sh --base=... --caos-std-path=<path to caos std>`. This installs everything
- We start the conversation commit from this repo's default branch, or whatever branch the user chooses. The repo's TREE becomes the conversation's content, at the root — its readmes, its agents.md and its `.caos-secrets` are conversation files, not a source tree. A fresh conversation therefore carries the client repo and nothing else; there are no source trees until something is imported.

The user will then say something like "import <repo name>" and the agent will find the repo in github, import it as a source tree in the conversation commit and then start working on it 

# Codebase

## Before committing

Run `time result/bin/caos-cli run-tool caos-test`

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
- `stage` is the worker's own POSITION, set only by its own curries, and never a caller's choice of what to do. Keep the two apart even when one arg could carry both: the cargo worker takes a `mode` for that (`all` selects the per-crate decomposition), and `all` is not a stage anything is up to. Tracing names a node by its `stage`, so an arg that mixes the two names a node after a request rather than after what it is doing
- A worker shall not tell its stages apart by which args happen to be present. There is nothing to read, so the node cannot be named -- and a curry that forgets an arg silently runs the wrong stage instead of failing

# Server

- Has a local git repo
    - GC is disabled
    - Every stored commit has its full ancestor history and ordinary tree
      contents. Gitlinks name separate histories, which are imported separately.
      Object uploads require dependencies first; Git transfers are verified
      before their objects become visible. The publication check validates new
      objects and checks links into the existing store by type, without walking
      that stored history again. Startup rejects pre-existing incomplete history
      rather than silently accepting or deleting it.
- shall listen on port 80 and respond to requests:
    - Git push/pull requests are routed to git to handle against the repo
    - WorkRequests as described below. The input is the hash of the ArgTree as a git tree (the stack travels alongside it, not inside the hashed tree). The result is the hash of the WorkResult

# Tracing

For any run, the server records the following in redis, in a single entry that is keyed off the hash of the ArgTree and is an append-only array of typed json fields:
- requested: with a time
- started: with a time
- ended: with a time and result (not the value, just success or failure)
- child: a named child arg tree that was requested (one for a run-then, 0 or more for map-then, etc)
- continuation: the continuation promise type (run, map, eval, etc) the arg tree of the continuation handler
- out-trace: any perf data that the worker chooses to leave behind in /cas/out-trace

These fields are enough to fully express what happened:
- If a parent arg tree depends on something that ended before its work started, then it was a cache hit
- If it depends on something that started before and finished after, then it requested something that someone else had already started
- If the child starts after the parent, it probably started because the parent requested it -- but the parent definitely waited for it
- If the child started after the parent finished (including its continuation), then the child was evicted and later rerun

To determine how two runs differed, we can diff their traces and see where the keys differ and how long each work took

The server supports pulling current trace data for a run: `GET /status/<arg tree hash>` returns a json tree. For any arg tree:
- If the work is done and there's no completion arg tree, the entry is skipped. Not "no promise": a continuation with no `then` has a promise and nothing left to point at
- If there's a completion arg tree, use it. Not gated on the work being done -- `ended` covers the continuation, so a node whose `then` is still running is not done, and gating would hide the `then` for exactly as long as it ran
- If there are child arg trees, render the parent, with an array of rendered children. (The finished ones will be ignored)
- Otherwise, render this node

Rendering a node. A node's name is the first of these that it has:
- The first line of its help argument. Base args are searched recursively until one is found
- Its `stage` argument -- the arg a multi-stage worker already uses to say which stage it is up to (see "Worker scripts") is also the name of what it is doing. `help` cannot reach these: a stage curries from `base` and forwards a chosen few args, so the tool's `help` is a sibling that is dropped at the first hop
- A short form of the image the base names. The full docker ref is the same sixty characters for every node of a fan-out, so it only displaces the part that differs

A name is qualified by a prefix, `<prefix>: <name>`:
- Following a completion, the prefix is the name of the node we came from -- a continuation handler is the same work one stage on. Set once and carried, not accumulated, so a long promise chain reads `<the tool>: fanout` rather than every stage concatenated
- Descending into a map-then or eval child, the child's own name from the parent replaces the prefix: it is separate work, not a later stage. A run-then or exact-request child takes no prefix, because the name there is the child's position in the continuation rather than a description of it, and such a child usually names itself

Names are for display and may be truncated to fit; the arg trees are what forensics reads

Each rendered node reports its `requested` and `started` times in milliseconds relative to the root request's `requested` time -- the root reads 0, every other node reads how long after the run began it reached that state, and work reused from an earlier run reads negative. The stored record keeps absolute times (they are compared across runs and processes); the relative ms are a rendering of them, for a human reading one run's tree

`GET /status/<arg tree hash>?all=1` asks what HAPPENED rather than what is happening:
- Nothing is skipped for being finished, and a completion arg tree is rendered as a CHILD of the node that promised it rather than replacing it. The shape is then the run's actual structure, which is what one run is diffed against another
- A node whose record ended before its parent started was REUSED by this run rather than performed by it. It is marked and not descended into: its children belong to the run that did the work, and following them would splice another invocation's tree into this one

While the cli is running something with `run` or `run-tool`, it uses `/status` to show the status of the work

# Building and testing caos, including inside caos

Caos can be built and tested on a host with just what's defined in flake.nix with ordinary commands like:
- `nix build`
- `result/bin/caosd up`
- `result/bin/caos-cli run dev/run-tests` (`dev/run-tests` is a caos expr that depends on /tests and runs them all on the current stack. If you run this on the host, you use the host stack. But the world test might fail. The normal usage inside the test container.)

The build and test tools use `dev/test-stack --tree=<hash> --command=<command>` to run the build steps in a worker. The goal is isolation from the host stack, with enough caching to make this fast. test-stack is a worker that:
- Mounts a persistent volume at /mounted-nix, copies anything missing in /mounted/nix/store from /nix/store and then mounts /mounted-nix at /nix
- Mounts the docker socket (to cache images) and a volume for git (single directory on the host is shared for between all test stacks). It uses the host's redis since we want to share the caches between stacks but can't have multiple redis proccesses using the same files
- Runs `caos get -r <hash>` to fetch the requested tree #todo still working on this
- Runs the provided command in the container and exists with the exit code of the command
- Uses --rm to remove the container after it exits

We use a single short-lived test worker with persistent data. This weakens test isolation, but we already expect tests to tolerate other tests' data (because it was too slow to start a fresh stack per test)

The build and tools are:
- `std/caos-build <tree-oid>`: `run-in-test-container  --tree=<treeoid> --command="nix build"`
- `std/caos-test <tree-oid>`: `run-in-test-container --tree=<treeoid> --command="nix build && .../caosd up && .../caos-cli run dev/run-tests"`

Some tests need to remain running/block while their child workers run. (Examples: anything that calls `caos-cli run`, and anything that needs to start a daemon that a worker talks to.) `--max-parallel` on the suite's `map-then` bounds how many tests are in flight (default 8), so the general pool has room for both tests and their children.

There should be exactly one copy of the code that starts a stack and builds the built-ins

# Misc

- `run-tool` does not fetch the output of the tool that it runs. It just prints the hash and the stdout part
- `caos put` checks whether the server has each object while descending the tree, to avoid putting things that are already there
- `refs/caos/bins` does NOT exist. Nothing creates it and nothing reads it: a
  tool gets the tree under test and builds from source, so there is no ref of
  prebuilt host binaries to resolve. Do not reintroduce one.


# From agents

Agents, add more notes here

# Merging and conflict resolution

An agent merges into a selected source tree using
`merge --theirs=<ref|hash>`, then resolves conflicts by producing ordinary
source tree commits. There is no index or staging step in the worker.

[Chat v3](design/chat.md) defines the conversation and source tree histories,
their refs, reconciliation, and publication. Conversation commits hold
protocol state; source tree commits hold code. They are connected by hashes
in conversation records, never by parent edges.

File tools, grep, and bash start at the conversation root and traverse
commit-valued source-tree entries. Bash can edit ordinary conversation files
and several source trees together. Source-tree directories preserve commit
identity through `mv` and `cp -a`; changes produce child code commits.
Repository tools are addressed with `run_tool` and one conversation-relative
path. Git operations such as merge still name their target source tree.

The TUI uses a separate harness checkout and explicit worker image arguments.
Attached repositories provide code, instructions, and tool schemas.
See [Chat](design/chat.md) for storage, reconciliation, and publication.

## Tools thread a commit, not a tree

Code mutations retain their input commit as a parent; merge retains both
inputs. Filesystem tools pin a conversation snapshot and preserve each
contained source tree's commit boundary.

- **Read-only tools** (`read`, `ls`, `grep`) return the input
  commit UNCHANGED — no new object, no no-op commit.
- **Mutations** (`write`, `edit`, `bash`) create a single-parent
  code commit for each changed source tree. Ordinary conversation-file
  changes stay in the conversation tree.
- **`merge`** returns a two-parent commit `commit(merged tree, parents =
  [input commit, theirs])` — the only tool that fills a second parent, but
  otherwise an ordinary tool with the ordinary signature.

A tool's proposal is reconciled against the selected source tree pointer using
the rules in [Chat v3](design/chat.md). Accepted source tree commits
preserve the mutation and merge ancestry. Equal trees do not make that
ancestry redundant. Read-only proposals leave the source tree unchanged.

Publication preserves the selected source tree's code history. Conversation
records (prompts, tool calls, and results) stay on the conversation branch;
publishing them is deferred.

The worker command `caos import-git <https-url> <commit>` posts that exact
commit to /git/import and prints its hash. It optionally reads a token file
specified by --github-token-file=PATH and forwards it in a sensitive header.
Ref resolution, provenance, and conversation attachment belong to callers.

The inline `import_source(source, revision?, into)` tool resolves a remote
revision in the agent container. Before importing objects it records the
commit and provenance as an import.json payload in tool.start. Inline starts
may omit a compute task; ordinary dispatched starts still name their task.
A resumed call uses the saved hash, and concurrent attempts accept the first
persisted observation. A new tool call observes the remote again.

After import, the tool atomically adds the gitlink, sibling .source.json
provenance and completed tool result. It rejects occupied destinations.
The tool is available in an empty conversation. Local paths use /import.
Only github.com on the default HTTPS port receives the mounted GitHub token
automatically.

## Remote Git imports

POST /git/import accepts only source (an HTTPS repository URL) and commit
(a full commit hash), with an optional X-Caos-Git-Token header. It imports that
exact commit, trees, blobs, and complete ancestor history directly into the
server's bare object store and returns {"commit": H}. Branch names, local
paths, and shallow imports are rejected. Callers resolve refs and persist
their chosen commit before invoking the endpoint.

Completed imports supply negotiation tips. A completion marker for the
repository and commit avoids fetching or checking history again. This is an
object-availability operation: a cache hit does not recheck remote access,
just as reading an available hash through the object API does not.
Token values never enter Git objects or completion markers.

The endpoint does not edit conversations or create import refs. Automatic GC
remains disabled; future GC must retain imported commits because gitlinks
and completion markers do not root them.

See [Git import endpoint](design/git-import.md) for the request flow and
limits of commit-based fetch negotiation.

## `merge --theirs=<commit>`

- Takes exactly one commit arg (`theirs`). The other side (`ours`) is the
  selected source tree commit at dispatch, including earlier accepted edits.
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
- Clean merge → `M`'s tree is the merged source tree and the merge is done.
- Conflicts → `M`'s tree carries inline conflict markers in the text files
  (what the agent edits), plus a reserved `.caos/conflicts` file (below). The
  agent resolves over subsequent turns; each resolution is an ordinary
  mutation commit on top of `M`.

## Resolving `--theirs`

The normal client does not publish a map of local branch names to workers.
Import the desired Git revision explicitly, then pass its full commit hash to
`merge`. The selected source-tree path identifies `ours`; `theirs` identifies
an immutable commit already available in CAOS. Importing a newer branch tip
does not merge it automatically.

A custom harness may supply a `merge-refs` map for named targets. Those names
resolve to the supplied snapshot, not live remote refs. Without that map,
names such as `main` and `origin/main` are unavailable; commit hashes still work.

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
`.caos/conflicts` means resolution is done. Recording an edited source tree
removes its empty ledger and prunes the `.caos` directory if empty. Bash and
inline edits share this rule. Unchanged commits, unresolved entries, other
metadata, and ordinary conversation files are preserved.

`.caos/conflicts` lives in the source tree, alongside the code. Inline
file tools can edit it; compute tools receive it with the rest of the
source tree. Conversation protocol files live in a separate tree, so no
step metadata needs to be injected or preserved in the source tree.

Publication rejects any `.caos` content in the final source tree, including
an empty `.caos/conflicts` or empty `.caos` directory. The check
distinguishes unresolved conflicts from completed cleanup and does not rewrite
the published commit. Earlier commits retain their conflict scaffolding.

Both `.caos/conflicts` and the inline markers sit in the diff the whole time,
so a mid-merge head is fully reviewable.

## Reading by hash (`read`/`ls` with `root`)

Both file readers default to the conversation root but accept a `root` — a
commit, tree, or blob hash — to read/list as of another revision. A commit or
tree `root` navigates its tree by path; a bare blob `root` (no path) reads that
object directly. This is load-bearing, not a convenience: the stage oids in
`.caos/conflicts` name content that is NOT reachable through any source tree path
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
  remaining marker is PUBLISH (the tui's PR or branch flow) — the moment work
  actually leaves the conversation.

## Publication

The [conversation publication flow](design/chat.md) publishes one named gitlink
with `/pr <gitlink> <base-remote-branch> [remote-URL]`. Its full path is the PR
branch name. The base is explicit; an omitted URL comes from unambiguous import
provenance. Directory ordering guides review, not publication. Publish earlier
PRs first, then name their remote branches as later PR bases.

The client previews the exact source commit and destination before confirmation.
If the source does not contain the fetched base tip, it offers to import that
base and send the agent a merge/rebase and test request. This action publishes
nothing; run `/pr` again after integration. Successful pushes and PR operations
are recorded as CAOS transcript entries. No snapshot has a special working or
sealed state.

Per-mutation commits remain in the published source tree history. Only the
previewed PR tip is checked for unresolved conflicts and reserved state;
intermediate commits may contain conflict markers or fail to build.
