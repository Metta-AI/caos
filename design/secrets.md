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
# Reader with no args
reader=std/github-push
# Reader with args. `=`, `@=`, etc are valid here. Paths are evaluated with `caos eval-path`
reader=tools/deploy -- --repo=github.com/me/proj
```
- A reader is a partial arg set. It is evaluated against the tree at the time that `caos-cli run` or similar is called
- A secret is visible to a worker if the worker's arg tree is a superset of one of the arg trees of the secret
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

Correctness requirements: Neither the secret values nor the names of the secrets are part of the work's cache key. Thus:
- A worker must fail if the secret is missing or invalid
- If the secret is present and valid, the worker's result must not depend on the secret's value
- For example, a worker can fail with an invalid secret, but it should not return a list that is filtered to what is visible to this secret's account

Server behavior:
- The server passes the list of secrets and the tree against which to evaluate them from one work request to the next, along with the stack
- When dispatching a work request, at the same time that the server selects a runner based on the closest match of the arg tree, the server also finds all secrets that list arg trees that are subsets of the worker's arg tree, and provides them to the runner

Note that this means that the server sees all secrets. We can revisit if this becomes a problem

## Remaining work

Built so far, in an interim shape: out-of-band injection at `/secret/<name>`,
superset matching (both `std/<name>` and tree-relative readers), the hard
output-scrub assertion (new objects only, refused at `caos put` before
publish), and best-effort log masking. What that version does differently from
the design above, plus what is unbuilt:

- **Carry the store as ephemeral run context.** Today the store is a server
  property (`CAOS_SECRETS_DIR`), read at dispatch. The design wants it threaded
  through the run like the stack — client → server → each promise sub-run — so
  secrets are per-user and can be iterated locally. The tree a reader evaluates
  against travels with the store (the store pins its own tree, which is what
  makes tree-relative readers safe); this replaces the interim server-side pin
  (`.tree` file / `CAOS_SECRETS_TREE`). Matching + injection stay server-side at
  every dispatch (so no-delegation holds: a sub-worker is entitled by matching
  its own arg tree, never by inheritance).

- **Entropy + cache isolation.** The required `entropy` field and the
  `/cas/args/secret-hash` entry — `H(sorted (name, entropy) pairs)` over the
  secrets a run is *actually granted*, folded into the arg tree only when a
  grant fires (match the base arg tree first, then append the hash, so it does
  not chase its own tail). Without it, two users with different secrets share
  cache entries, and a misconfigured user silently hits another's result. The
  hash — not the raw entropy — goes in the tree, because an id is a bearer
  capability for the cache: knowing it reconstructs the key of any run that used
  it. Rotating `value=` keeps the entropy (cache preserved); rotating the
  entropy re-namespaces the cache (what to do on a suspected id leak).

- **`name=` field.** The worker-visible name, distinct from the filename (which
  stays the user's own label); both the hash and the `/secret/<name>` mount use
  it. Currently the filename is the name.

- **Entropy tooling.** A `caos secrets`-style command over the dir that fills a
  missing `entropy` with fresh entropy and warns (or refuses) on a low-entropy
  one — so the safe default is automatic and a misconfig degrades to a cache
  *miss* (a fresh, uncached run), never a cross-hit. Load-bearing: a
  low-entropy id is brute-forceable out of the hash.

- **Reader `:@=` args and binary `value:@=`.** `:@=` resolves inside a tool's
  `.caos-expr` but not yet on a reader's own trailing args; `value:@=` is read
  but kept UTF-8 (binary/multiline later).

- **`run`-form `.caos-expr` grants** are deliberately unresolved (a grant must
  never trigger compute); likely permanent.

- **Shared-server exposure.** Carrying the whole store means a shared server
  sees values it never injects (sub-runs aren't known ahead of time, so the
  client can't pre-filter to the granted subset). Moot for a per-user/local
  server; a tighter hand-off is future work.