# First-class commit objects — design note

**Status:** implemented (alongside run-then; see `design/map-then.md`).

Values used to be `blob | tree | promise`, with commits peeled to trees at
every boundary (`resolve_ref`, ref args). A commit is now a value in its own
right — groundwork for an agent harness where each conversation turn *is* a
commit: message = the turn's text, tree = workspace state, parent = the
previous turn. An llm-step worker receives the conversation head commit as an
arg, executes tool calls as run-then sub-runs, and mints step/turn commits.

## The pieces

- **Storage.** `POST /object` (and the git transport's `put_object`) accepts
  the `commit` kind: the encoding is validated with gix, but the *raw posted
  bytes* are stored, so the hash is exactly what the client computed. `GET
  /object/<hash>` always served any kind.
- **Results.** A worker result `commit <hash>` flows through `run_req`, the
  Redis cache, and `refs/caos/res/<req>` unchanged (a result was always an
  opaque `"<type> <hash>"`). Where a result lands *in a tree* — a map child,
  run-then's `--result` — it becomes a **gitlink** entry (mode 160000). Git's
  reachability rules don't traverse gitlinks, which has two consequences we
  accept deliberately:
  - workers fetch objects by explicit hash over `/object`, so the don't-fetch
    semantics never bite inside caos;
  - a request's own push does **not** carry a commit arg's closure, so the
    client pushes it separately (see args below).
- **Args.** The default arg forms keep peeling (image refs and `std`
  resolution depend on it). The explicit, unpeeled opt-in is a new arg *type*
  in the existing `--name[:type]=value` grammar: **`--name:commit=value`**.
  The value is a bare commit hash (either client), a `/cas` path recorded as a
  commit (worker), or a revspec such as `HEAD` resolved in the working repo
  (CLI). Resolution `ensure_pushed`es the commit's closure, since the gitlink
  won't (one negotiated push; a turn commit whose tree is mostly unchanged
  ships only its delta).
- **Worker-side materialization.** In `/cas` a commit is a *file holding the
  raw commit object* (headers, blank line, message), tagged
  `user.caos.kind=commit` — the same `KIND_XATTR` mechanism promises use, both
  on the unfetched placeholder and after `get`. That keeps a commit-valued
  path distinguishable from a blob, so passing it onward (`--x:@=/cas/head`, a
  curry binding) re-emits a gitlink rather than a mis-typed blob entry.
- **Minting.** `caos put-commit <src-file> <cas-path>` stores the file's bytes
  as a validated commit object, records the kind-tagged placeholder, and
  prints the hash; written at `/cas/out` it makes `commit <hash>` the run's
  result. `caos hash <cas-path>` prints a path's recorded hash — a minted
  child needs its parent's *id*, which no fetched content contains.
  `worker_common` wraps these: `Commit {tree, parents, message}`,
  `read_commit(cas_path)`, `write_commit(tree, parents, message, out) -> hash`,
  `cas_hash(cas_path)`. `write_commit` uses a fixed identity and zero
  timestamp, so a commit is a pure value of `(tree, parents, message)` —
  identical turns dedup like any other object.

## Non-goals (for now)

- No checkout form for a gitlink inside a result tree (`caos-cli` still
  refuses; a *top-level* commit result streams/writes its raw bytes, and the
  real object is a `git fetch caos <hash>` away).
- No tag objects; no commit *ingestion* from host paths (`--name:@=` stays
  tree/blob) — the `:commit=` forms cover the harness's needs.
