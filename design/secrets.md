# Secrets: identity-is-capability — design note

**Status:** proposed. Design discussion only; not built. Builds on
`.caos-expr` (eval-path, deep-deps) and map-then (server-mediated worker
starts).

## Problem

Some tools need secrets: the github-push tool needs an auth token, and there
will be many like it. But:
- we don't want secrets in content-addressed stores, where they might leak
- we don't want secrets in keys, because we don't want to invalidate (most) keys if a secret is rotated
- we don't want secrets in one worker/arg tree to be able to be read from it by another worker

## Solution

`.caos-secrets`:
- Secrets live in a git-ignored .caos-secrets directory
- Each secret file contains the secret's value and a list of partial arg trees. This is formatted as a repeated-key file. For example:
```
# Inline secret
value=<secret key>
# External key
value:@=<file containing key>
# Reader with no args
reader=std/github-push
# Reader with args. `=`, `@=`, etc are valid here. Paths are evaluated with `caos eval-path`
reader=tools/deploy -- --repo=github.com/me/proj
```
- A reader is a partial arg set. A secret is visible to a worker if the worker's arg tree is a superset of one of the arg trees of the secret 

Worker experience:
- If a secret is visible to a worker, it is injected into a worker in `/secret/<name>`
- We attempt to scrub secret values from the logs of workers
- We attempt to check files that are added to git with `caos put` for secret values. Any new file (hash not in git) that contains the value of a secret that is visible to this worker is rejected

Correctness requirements: Neither the secret values nor the names of the secrets are part of the work's cache key. Thus:
- A worker must fail if the secret is missing or invalid
- If the secret is present and valid, the worker's result must not depend on the secret's value
- For example, a worker can fail with an invalid secret, but it should not return a list that is filtered to what is visible to this secret's account

Server behavior:
- When dispatching a work request, at the same time that the server selects a runner based on the closest match of the arg tree, the server also finds all secrets that list arg trees that are subsets of the worker's arg tree, and provides them to the runner

## Bot version follows

## Declaration: secrets as deps

A tool declares the secrets it uses in its deep-deps file, alongside ordinary
deps, one line each:

```
secret/github
```

This is **intent, not authority**. Its jobs: visible in review ("this tool
uses github"), tells the server which secrets to inject and scrub for this
run, and it is the audit anchor. Declaration alone never resolves a secret —
otherwise capability is self-granting (anyone writes `secret/github`).

## Grant: the `.caos-secrets` registry

Secrets live in a git-ignored `.caos-secrets` (file or directory), never
committed. Each entry lists a secret's **name**, its **value**, and **who may
read it** — the reader is a **partial arg tree**, and any worker whose arg
tree is a **superset** matches (the grant states the minimum that must hold;
unspecified args are wildcards).

To keep the file human-readable it lists **paths, not hashes** — workers
defined in the tree and in std, e.g. `std/github-push`. These paths are
resolved with `caos eval-path` (against the **same tree revision the work runs
from**, so grant and running tree canonicalize to the same normal form:
paths → content hashes). The superset match is over that canonical form, so it
stays precise despite being written as names.

Resolution succeeds only when **both** hold: the running code *declared* the
secret and the registry *grants* it to that identity. Declaration without
grant = refusal; grant without declaration = misconfigured tool (nothing to
inject or scrub).

This is **identity-is-capability**, not possession-is-capability: the registry
key is *what the code is* (rederived by eval-path), not a bearer token the code
holds. Closest priors are workload identity / attestation, not
object-capabilities; the caos twist is that the "measurement" is just the
content hash and the verifier rederives it — no issuer, no signing.

Payoffs: "who can read `github`?" is answerable by enumerating registry
entries (over the *registered* corpus — you can't enumerate over all arg trees
anyone might build, which is itself why the grant must be a registry, not a
free-floating declaration); revocation is editing the registry, not hunting
leaked copies.

## Start-time injection

The server never needs to authenticate a "who's asking?" request mid-run,
because **there is no later**: every worker is born from a server-mediated
start, and at that instant the server holds the exact arg tree it is
dispatching. It checks grants and injects **then**, once. A sub-worker is not
an exception — it is another server-mediated start, checked independently.
This is what enforces no-delegation for free.

- **Injection is out of band** — an env var, or a file the server drops into
  the container — *never* an arg (that would put the value back in the tree).
- `caos get-secret github` just reads what the server pre-placed, and only
  what this run was entitled to.

**Load-bearing invariant:** every worker start goes through the server. No
fast-path may let a worker spawn a child directly, or that child's start is
unchecked and the model has a hole. (map-then already routes every start
through the server.)

## Caching and effects

Because secrets are invisible to the tree, **secret access only happens on a
cache miss** (hit ⇒ worker never runs ⇒ `get-secret` never called). For a
private *fetch* this is correct: the token isn't in the key, so rotating it
doesn't bust the cache and the fetched content didn't change.

But it makes **effects** mandatory to handle: a *push* has no key-varying
input, so it would cache and no-op on the second call. So an
**uncacheable/effectful-run** notion is required for pushes to run at all. The
effect flag is **orthogonal** to the secret machinery, not implied by it;
"declares a secret + is effectful" is just the common shape.

Caveat: revoking a grant does not retract outputs already cached while it held
(moot for effectful pushes; the output-visibility question for fetches).

## Leak prevention

Two mechanisms, in order of trustworthiness:

- **Output assertion (hard).** For the secrets this run was injected, scan its
  output for the raw bytes and **fail the run** on a hit. Only **new** objects
  need scanning: an output object that dedups to something already in the
  store can't be a leak introduced by this run (if the secret's bytes already
  matched a stored object, the leak predates this run). Multi-string search
  (Aho-Corasick) over new blobs only — cheap and complete for raw-byte leaks.
  Scan at the **ingestion boundary, before publish**: quarantine new objects
  until scanned, and on a hit never make them readable. Outputs can be
  *refused*; that's what makes this stronger than log masking.

- **Log masking (best-effort).** The server knows the exact byte-strings it
  handed this run, so it scrubs *those* from this run's `server` / `runnerd` /
  `serve` logs — per-run scoped. Standard practice (GitHub Actions, GitLab CI,
  CircleCI, Jenkins all do it).

Honest limits (shared by both, and by every prior system): **transform-blind**
— a secret that is base64'd, JSON-escaped, or split across lines slips through;
these catch *bugs* (a token in an error string, a stray credentials file swept
into output), never a determined exfiltrator inside the trust boundary, which
has the plaintext and unbounded encodings. **Low-entropy secrets** are
second-class — they either match everywhere or can't be scanned safely.

## Residual trust (documented, not solved)

- The entitled tool sees the plaintext — unavoidable, it must authenticate.
  What the design buys: the value reaches only that worker, only for that run,
  and never lands in the store, the cache key, or a replayable tree.
- A grant is only as strong as review of the named path and its transitive
  deep-deps: whoever can land a change to `std/github-push` redirects the
  secret. The audit story is "review these paths," not "trust this hash."
- **Superset match trusts** that feeding the entitled image *extra* args can't
  subvert it into exfiltrating (unknown args ignored; no arg swaps out the code
  path). A real assumption about how the entitled tool reads its args.
- Output visibility for private *fetches* is a separate policy decision
  (anyone with store access can read the fetched content), not part of secret
  handling.

## Open items

- **Grant granularity** falls out of the partial-tree superset: pin only the
  image ⇒ any invocation of that tool; pin more args ⇒ narrower. One
  mechanism, no separate knob.
- **Composed identity** as a deliberate escape hatch for the generic-conduit
  case (a curried tree granted as a unit) — resisted until something concrete
  needs it.
- Exact injection channel (env vs. dropped file) and the `.caos-secrets`
  directory layout.
