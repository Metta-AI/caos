# Secrets: identity-is-capability — design note

**Status:** partly built. The store is carried as ephemeral run context and
resolved client-side; injection, superset matching, the entropy/`secret-hash`
cache-isolation tag, the output-scrub assertion, and log masking all exist. The
**caller-propagation** refinement below (fold `secret-hash` at image-eval, not
at run assembly) and the injection double-check are designed but not yet built,
as is the entropy tooling. Builds on `.caos-expr` (eval-path, deep-deps) and
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
- The entropy field is required and must contain real entropy
- Each granted secret contributes its (worker-visible name, entropy) to a
  `secret-hash` entry folded into the worker's arg tree (visible at
  `/cas/args/secret-hash`). This makes two users with different secrets see
  different cache keys — but keys the secret's *value* out (so rotating a value
  doesn't bust the cache), and stores the *digest* of the entropy, never the
  entropy itself (the entropy is a bearer capability for the cache: knowing it
  reconstructs the key of any run that used it). The name is included because a
  different mount name would make the worker run differently.

### Where `secret-hash` is folded, and how it reaches callers

The `secret-hash` is folded in **one place: when the image is evaluated**
(`eval-path`). That is the single chokepoint through which every use of a worker
passes — `run` resolves its image, `curry` resolves the base it curries onto,
deep-deps resolves the dep it mounts, and an embedder resolves the ref it
embeds — so folding it once, at eval, gives every downstream user a per-user
oid for free, and their arg trees (and keys) differ all the way up. This is what
isolates *callers*, not just the leaf: `deploy`, which embeds a per-user
`github-push`, is itself per-user, and so is everything that embeds `deploy`.
No touched-propagation and no per-caller cache invalidation — same-secret users
still share, only distinct-secret users diverge.

Two things this depends on:

- **Match the curry-unwrapped entries, not the bare image.** Evaluating a curry
  node `github-push --repo=x` unwraps to `{image, repo}`, so an arg-pinned
  reader (`github-push -- --repo=x`) folds at eval too — one place covers both
  reader shapes, and `curry` needs no fold of its own.
- **Fold as a sibling of `image`, never inside the image tree.** The image tree
  hash must stay per-*content*, so `convert_git_image` (keyed on it) builds the
  docker image once for everyone; only the run/result is per-user.

Residual: a consumer is isolated only if it reaches the worker **through eval**
(structurally — curried, a dep, or an eval'd ref). A worker that caches a
*pre-eval* ref and reuses it across stores without re-evaluating would escape
this — which is exactly the `std`-removal boundary (reach a tool as a dep, not
as an ambient `/cas/std/...` string), not a separate hole to fortify.

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
(readers via eval-path, so the server never evals); out-of-band injection at
`/secret/<name>`; superset matching; the `entropy`/`secret-hash` tag; the hard
output-scrub assertion (new objects only, refused at `caos put` before publish);
and best-effort log masking. `name=` is supported.

What that build does differently from the design above, plus what is unbuilt:

- **Move the `secret-hash` fold to image-eval.** Today it is folded at *run
  assembly* (client `assemble_arg_tree` / server `run_image`) — downstream of
  eval and off to the side, so the eval'd ref a *caller* embeds carries no
  `secret-hash` and callers are not isolated. The fix is to fold it once, in the
  eval funnel, matching curry-unwrapped entries against the carried store, as a
  sibling of `image` (see "Where `secret-hash` is folded"). The real plumbing is
  getting the store into every eval — including **deep-deps**, which is itself a
  worker that resolves deps and so needs the store threaded into its own run.
  Reconcile with the run-time fold so a worker isn't folded twice differently
  (same name+entropy → same hash → the merge dedups; worth a test).

- **Injection double-check.** The server must inject only when the worker
  *already* carries the matching `secret-hash` (see "Server behavior"), not on a
  bare reader match — so injection can't happen under a key that doesn't reflect
  it. Today injection is a bare reader match.

- **Entropy tooling.** A `caos secrets`-style command over the dir that fills a
  missing `entropy` with fresh entropy and warns (or refuses) on a low-entropy
  one — so the safe default is automatic and a misconfig degrades to a cache
  *miss* (a fresh, uncached run), never a cross-hit. Load-bearing: a low-entropy
  id is brute-forceable out of the hash.

- **Reader `:@=` args and binary `value:@=`.** `:@=` resolves inside a tool's
  `.caos-expr` but not yet on a reader's own trailing args; `value:@=` is read
  but kept UTF-8 (binary/multiline later).

- **`run`-form `.caos-expr` grants** are deliberately unresolved (a grant must
  never trigger compute); likely permanent.

- **Shared-server exposure.** Carrying the whole store means a shared server
  sees values it never injects (sub-runs aren't known ahead of time, so the
  client can't pre-filter to the granted subset). Moot for a per-user/local
  server; a tighter hand-off is future work.