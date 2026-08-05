# Secrets: identity-is-capability — design note

**Status:** proposed. Design discussion only; not built. Builds on
`.caos-expr` (eval-path, deep-deps) and map-then (server-mediated worker
starts).

## Problem

Some tools need secrets: the github-push tool needs an auth token, and there
will be many like it. But caos is built on **content-addressed, cached arg
trees**, and a secret is exactly the thing that must not enter a
content-addressed store or a cache key. So the value must stay out of the
tree; only the secret's *identity* may appear.

## No secret arg kind

We considered `--foo:secret=bar`. Rejected. If no arg kind ever carries a
secret value, then **no secret value can ride in an arg tree**, so there is no
delegation-by-passing: a worker cannot hand a token to a sub-run, because the
only channel would be an arg, and args are hashed and cached. Entitlement is
therefore **always per-identity, never transitive** — a child that needs
github must itself be entitled; it can't be lent the value by its parent. Same
discipline as removing `std`: capability doesn't leak downhill through
composition.

Consequence to accept: a generic authenticated-conduit worker (a shared
`http-post` many tools route through) cannot work — the code that touches the
plaintext must be the code that is entitled.

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
