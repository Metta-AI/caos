# Client-owned conversation refs

**Status:** implemented.

## Motivation

Conversation history is Git history. A conversation client or worker can read,
create, and compare-and-swap that history with ordinary Git operations, so the
CAOS server should not understand conversation ref names, event shapes, or
append-only policy. The server's job is to execute content-addressed work and
serve a Git repository; conversation coordination belongs to the clients that
define the protocol.

Before this cleanup, that boundary was blurred in three ways:

1. `llm-step`, `run-and-update-ref`, and the host chat client call CAOS-specific
   `/ref/read`, `/ref/append`, and `/ref/transaction` endpoints.
2. The server installs a conversation-aware pre-receive hook and repairs
   conversation heads with special append-only semantics.
3. Async conversation reconciliation discovers a task result indirectly through
   the server-owned `refs/caos/res/<request>` pin.

The cleanup moved all three responsibilities out of the server. We explicitly
accept that a buggy or malicious worker with repository access can rewrite
conversation refs; the server provides transport, not policy.

## Target boundary

The server retains only generic facilities:

- Git smart HTTP upload-pack and receive-pack.
- Object/CAS transport used by ordinary CAOS computation.
- Generic top-level request result pinning under `refs/caos/res/*`, which keeps
  successful results reachable and predates conversations.
- Generic repository repair based on recoverable Git reachability and reflogs.

Conversation clients and workers own:

- exact conversation-ref reads with `git fetch`;
- optimistic single-ref updates with `git push --force-with-lease`;
- multi-ref creation and membership changes with `git push --atomic` plus one
  lease per ref;
- event validation, append/retry policy, and conflict interpretation;
- carrying the completed result hash in terminal async events.

No conversation path queries `refs/caos/res/*`. Those refs remain an
implementation detail of generic request execution rather than a conversation
result API.

## Runtime shape

Git is opt-in. The existing shared, seeded `runner` image remains unchanged for
ordinary Rust workers. A separate literal-flake `git-runner` contains a small
interpreter plus `gitMinimal`; the ordinary flake-builder builds it lazily, so it
does not widen the irreducible seeded core. Only these std entries ask `rustc`
to wrap their compiled binary with it:

- `llm-step`
- `run-and-update-ref`

The Rust binaries invoke `git` directly. Small process and repository helpers
may live in `worker-common`; they are library code and do not add Git to any
image. Selecting `git-runner` is explicit in each consumer's `.caos-expr`, so it
is visible in the ArgTree and therefore in the computation key.

## Conversation Git protocol

Each worker creates a throwaway local repository and points its `origin` at
`CAOS_SERVER_URL`.

- Read one ref by fetching that exact source ref into `FETCH_HEAD`. Protocol v2
  communicates the exact prefix to upload-pack; shallow, tree-filtered fetches
  avoid downloading the conversation workspace or its history.
- Store event objects through the ordinary content-addressed object API, then
  fetch the candidate object into the scratch Git repository so it can serve as
  a push refspec source without downloading its workspace closure.
- Append with `git push --force-with-lease=<ref>:<observed> <new>:<ref>`.
- Create or modify several refs as one operation with `git push --atomic`, one
  explicit lease per ref, and one refspec per update.
- After an ambiguous push failure, fetch the ref again. Treat the operation as
  successful if the desired object is visible, as a lost race if the head
  changed, and as an infrastructure failure if the observed head is unchanged.

The server does not enforce first-parent ancestry, workspace-tree continuity,
or conversation namespaces. The clients continue to enforce those invariants
because they consume the protocol.

## Async result ownership

Before this cleanup, an async terminal event recorded only:

```json
{"async":{"task":"<request>","status":"complete"}}
```

and `llm-step` later queried the request's result ref. A terminal event now
records the result it announces:

```json
{"async":{"task":"<request>","status":"complete","result":"<object>"}}
```

`failed` events carry the hash of the small failure result tree produced by
`run-and-update-ref`. `pending` events carry no result. A terminal event is
published only after its result object exists, making the conversation log
self-contained for reconciliation. Compatibility with terminal events that lack
`result` is intentionally not retained.

Generic execution continues to pin the same result under
`refs/caos/res/<request>` for durability and Git negotiation. Conversations do
not read that ref and do not depend on its naming or timing.

## Delivery stack

1. **Opt-in Git runtime — landed.** The flake-built `git-runner` is bound only
   from `llm-step` and `run-and-update-ref`; shared Git command helpers do not
   add Git to the ordinary runner image.
2. **Client-owned conversation operations — landed.** Production conversation
   ref operations use Git fetch/push, and terminal async events carry result
   hashes.
3. **Remove server specialization — landed.** The specialized ref API,
   conversation pre-receive validator and installed hook, and append-only repair
   classification are gone. Generic Git transport, result pinning, and crash
   repair remain.

The server upgrade removes a hook previously installed by CAOS before the
validator disappears. That cleanup is narrowly scoped to the hook file owned by
CAOS; administrator-owned hooks are left untouched.

## Completion criteria

- No production client or worker calls `/ref/read`, `/ref/append`, or
  `/ref/transaction`.
- No conversation path reads `refs/caos/res/*`.
- Only `llm-step` and `run-and-update-ref` execute in `git-runner`; the ordinary
  shared runner does not contain Git.
- The server exposes no `/ref/*` routes and contains no conversation-aware hook,
  ref validation, or repair policy.
- Concurrent appends still resolve through explicit leases and retries, and
  multi-ref updates remain atomic.
- The workspace test suite, flake source lint, and relevant Nix builds pass for
  each layer.
