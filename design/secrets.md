# Secrets: identity-is-capability — design note

**Status:** partly built. The store is carried as ephemeral run context and
resolved client-side; injection (gated by the double-check below), superset
matching over path-only readers, the entropy/`secret-hash` cache-isolation tag,
the output-scrub assertion, and log masking all exist and isolate the *running*
worker. **Caller-propagation** (isolating a worker's callers) and the entropy
tooling are not yet built. Builds on `.caos-expr` (eval-path, deep-deps) and
map-then (server-mediated worker starts).

## Problem

Some tools need secrets: the github-push tool needs an auth token, and there will be many like it. But:
- we don't want secrets in content-addressed stores, where they might leak
- we don't want secrets in keys, because we don't want to invalidate (most) keys if a secret is rotated
- we don't want secrets in one worker/arg tree to be able to be read from it by another worker

## Solution

`.caos-secrets`:
- Secrets live in a git-ignored .caos-secrets directory
- Each secret file contains the secret's value and a list of partial arg trees. This is formatted as a repeated-key file. For example:
```
# Optional name. Defalts to the name of the file. This is the name that is used in the worker for /secret/<name>
name=<name>
entropy=...
# Inline secret
value=<secret key>
# External key
value:@=<file containing key>
# A reader is a PATH to an expression — no argument pins. It is eval-path'd to
# an arg tree; a worker whose arg tree is a superset of that is granted.
reader=std/github-push
reader=tools/deploy
```
- A reader names an **expression** (a `/std/<name>` builtin or a tree path),
  evaluated with `caos eval-path` at `caos-cli run` time. Its resolved arg tree
  already carries whatever the expression bakes in (e.g. a curried `worker1`
  script), so it is as specific as the expression is. To narrow a grant (e.g.
  github-push for one repo), point the reader at a **narrower expression**, not
  at arg pins in this file — narrowing then lives in the content-addressed,
  reviewable expression layer, and the whole narrowed worker is what the
  `secret-hash` marks. This also keeps "secret-eligible" meaning exactly "the
  arg tree an expression produces," which is the one thing eval-path can stamp.
- A secret is visible to a worker if the worker's arg tree is a superset of one
  of the secret's (eval-path'd) reader arg trees.
- The entropy field is required and must contain real entropy
- Each granted secret contributes its (worker-visible name, entropy) to a
  `secret-hash` entry folded into the worker's arg tree (visible at
  `/cas/args/secret-hash`). This makes two users with different secrets see
  different cache keys — but keys the secret's *value* out (so rotating a value
  doesn't bust the cache), and stores the *digest* of the entropy, never the
  entropy itself (the entropy is a bearer capability for the cache: knowing it
  reconstructs the key of any run that used it). The name is included because a
  different mount name would make the worker run differently.

### Where `secret-hash` is folded

The `secret-hash` is folded wherever a **runnable arg tree is assembled** — the
client's `assemble_arg_tree` (a top-level `caos-cli run`, and eval-path's own
`run`, both go through it) and its server-side twin `run_image` (map-then/
run-then sub-runs, which the server assembles from a worker's continuation, never
through client eval-path). It has to be at assembly, not at image resolution,
because the match is against the *full* arg tree (a path-only reader
`std/github-push` resolves to `{image: bash, worker1: push-script}`, so
`worker1` must be present to match) — and because a worker runs either top-level
(client assemble) or as a sub-run (server `run_image`), and the server must fold
independently, since a worker reaches it by continuation, not by calling
eval-path.

Two things this depends on:

- **Match the curry-unwrapped entries.** A curried worker unwraps to its flat
  entries (`{image, worker1, …}`) before matching, so a scripted tool is
  distinguished by its script, not just its base image.
- **Fold as a sibling of `image`, never inside the image tree.** The image tree
  hash stays per-*content*, so `convert_git_image` (keyed on it) builds the
  docker image once for everyone; only the run/result is per-user.

This isolates the **running worker** (the leaf `github-push`, however reached,
gets its own per-user key and injection). It does **not** yet isolate *callers*:
`deploy` embeds `github-push` by a ref carrying no `secret-hash`, so `deploy`'s
own tree is identical across users while its resolved result holds
`github-push`'s per-user output. **Caller-propagation is unbuilt** (see Remaining
work): it needs the mark folded where an expression forms an *embeddable* tree
(curry / eval-path), the store threaded into eval-path, and — for dep-mounted
tools — into deep-deps.

Residual even then: a consumer is isolated only if it reaches the worker
**through eval** (a dep or an eval'd ref), not by caching a *pre-eval* ambient
`/cas/std/...` ref — which is the `std`-removal boundary, not a separate hole to
fortify.

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

Built: the store carried as ephemeral run context and resolved client-side
(path-only readers via eval-path, so the server never evals); out-of-band
injection at `/secret/<name>`, gated by the double-check (see "Server
behavior"); superset matching; the `entropy`/`secret-hash` tag folded at
arg-tree assembly; the hard output-scrub assertion (new objects only, refused at
`caos put` before publish); best-effort log masking; and `name=`.

What is unbuilt:

- **Caller-propagation.** Today `secret-hash` is folded at *run assembly*
  (client `assemble_arg_tree` / server `run_image`), which isolates the running
  worker but not its callers (see "Where `secret-hash` is folded"). Isolating
  callers needs the mark folded where an expression forms an *embeddable* tree
  (curry and eval-path's `curry`/`run` results), the store threaded into
  eval-path (which today assembles with an empty store), and — for dep-mounted
  tools — into **deep-deps** (itself a worker). Gated on the convention for how
  a dependent references a dep (source subtree vs marked arg-tree ref), which
  isn't built. Reconcile with the run-assembly fold so a tree isn't folded twice
  differently (same name+entropy → same hash → the merge dedups; worth a test).

- **Injection double-check.** *(Built.)* The server injects only when the
  worker already carries the matching `secret-hash` (see "Server behavior"), so
  a value can't reach a worker whose key doesn't reflect it.

- **Entropy tooling.** A `caos secrets`-style command over the dir that fills a
  missing `entropy` with fresh entropy and warns (or refuses) on a low-entropy
  one — so the safe default is automatic and a misconfig degrades to a cache
  *miss* (a fresh, uncached run), never a cross-hit. Load-bearing: a low-entropy
  id is brute-forceable out of the hash. Until it exists, a missing `entropy=`
  silently weakens isolation (the hash defaults to the empty entropy), so it's
  the highest-value remaining item.

- **Binary `value:@=`.** Read but kept UTF-8 (binary/multiline later).

- **`run`-form `.caos-expr` grants** are deliberately unresolved (a grant must
  never trigger compute); likely permanent.

- **Shared-server exposure.** Carrying the whole store means a shared server
  sees values it never injects (sub-runs aren't known ahead of time, so the
  client can't pre-filter to the granted subset). Moot for a per-user/local
  server; a tighter hand-off is future work.