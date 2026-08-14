# Plan: `--base`, typed arg locators, and remote git refs

Design decisions from the `.caos-expr` / consumer-story discussion, for review
before implementation. Nothing here is built yet.

## Motivation

Two problems, one shared mechanism:

1. **The consumer story.** A project that uses caos (as a nix flake input) must
   reach caos' `std/*` from its own root `.caos-expr`, which means the caos tree
   has to be *mounted into the project's tree* — but only into the **evaluation
   result** (the "expression's tree"), never committed. Today there is no way to
   name a tree that lives in another repo.

2. **Sniffing.** Image position (in `.caos-expr` and on the CLI) guesses what a
   bare token means by its *shape* — `docker://…` vs a hex hash vs a path. That
   violates the codebase's own rule ("the type is chosen by the operator, not by
   sniffing the value") that every *arg* already follows.

The fix for both is the same: **make "the thing we run" an ordinary typed arg
(`--base`)**, and **add a typed locator for a tree in another repo (`:@@=`)**
whose pin is a content hash, so it caches like everything else.

## Core principle carried through

A **URL is a name; a hash is content.** A remote ref MUST carry a mandatory
`rev` (a full commit sha), and the client resolves `url + rev → oid` **at eval
time**, fetching the closure into the CAS. From that point it is an ordinary
oid. **A URL never sits inside an ArgTree / cache key.** Resolution is a
**client** capability (workers stay pure and network-free — the security story);
the resolved arg entry is a plain tree/blob oid, so two consumers pinning the
same rev share the whole deepened subgraph by hash.

We borrow **nix's flake-reference string grammar** as the locator *syntax only*.
No nix runs; nothing evaluates a flake. It is just a well-specified string format
for "where a tree comes from," and it already covers remote-pinned and local.

## The arg-type taxonomy (final)

One parser, one `ArgType` enum, shared by the CLI (`lib.rs`) and the `.caos-expr`
evaluator (`eval.rs`). (Today these are **two divergent parsers** — the eval one
only knows `@` + literal, the CLI one also knows `commit` + `tree`. Unifying them
is a prerequisite and closes that drift.)

| form | meaning | representation in the arg tree |
|---|---|---|
| `--n=v` | literal string | blob |
| `--n:@=<path>` | a path into the **ambient** git repo, relative to the `.caos-expr` dir — **unchanged from today**, always a bare path, never a scheme | tree/blob oid |
| `--n:@@=<ref>` | a **proper nix-style ref** to a tree in another repo (or a local one) | tree/blob oid (resolved at eval time) |
| `--n:hash=<oid>` | an existing object referenced by hash (generalizes today's `:tree=` to blob/tree) | that oid |
| `--n:commit=<rev>` | unpeeled commit — **unchanged from today** | gitlink (mode 160000) |
| `--n:docker=<ref>` | a docker image ref | blob `docker://<ref>` |
| `--n=$VAR` | (eval only) an object a prior line produced | that oid |

Notes:
- **`:@=` vs `:@@=` — two operators, deliberately.** `:@=` stays a bare path
  (the very common case; no `path:` prefix to type). `:@@=` takes the full ref
  grammar. Local refs *can* be written `:@@=path:./x` / `:@@=git+file://…`, but
  you'd rarely bother because `:@=` already covers ambient paths.
- **`:docker=` / "stop sniffing" is a parse-time change only.** `:docker=alpine`
  still *stores* the internal blob `docker://alpine`, so `resolve_run_image`'s
  downstream `docker://`-prefix check is untouched. What dies is treating a bare
  token as an image by *shape*.
- **`:hash=` generalizes the existing `:tree=`** to accept a blob or tree by oid
  (verify it exists). `:commit=` stays separate (it is the *unpeeled* form).

## `:@@=` locator grammar (nix-style)

Canonical:

```
git+https://github.com/company/repo?rev=<40-hex>&dir=std/deep-deps
git+ssh://git@github.com/company/repo?rev=<40-hex>&dir=x
path:./some/dir            # local, no rev
git+file:///abs/repo?rev=<40-hex>&dir=x
```

Optional client-normalized sugar: `github:owner/repo?rev=<sha>&dir=x`.

Rules:
- **`rev` is mandatory for remote schemes**; it must be a **full-length hex
  commit sha**. A `ref=` (branch/tag) without a `rev` is an **error** — that
  rejection is what enforces the content-addressing invariant.
- **`rev` is absent for local schemes** (`path:` / a local dir) — local content
  is hashed live at eval time, honoring the existing "only what git tracks is
  visible" rule.
- `dir=` selects a subtree within the repo.

Why commit-and-then-path (not a bare subtree hash): **GitHub only serves
*commits*** (`allowReachableSHA1InWant`), so `git fetch --depth 1 <url> <rev>` is
the fetchable granularity; `dir` selects within it. The commit is also the pin.

Resolution (client, eval time): `git fetch --depth 1 <url> <rev>` → verify rev →
peel to tree → descend `dir` → ingest closure → **the resulting oid is the arg
entry**, byte-for-byte as if it had come from a local `:@=`. URL/rev are fetch
coordinates and provenance (trace), never part of the key.

Parser safety (verified against current code): the token is whitespace-free;
`split_once('=')` takes the **first** `=`, so `?rev=…&dir=…` survives in the
value; the `:`-type split runs on the **key** only, so `://` is untouched; and
`.caos-expr` comments are **full-line only** (`eval.rs` trims then checks
`starts_with('#')`), so a value may contain anything.

## `--base`: full collapse, no special arg, no sugar

"The thing we run" becomes an **ordinary typed arg** named `base`, written with
the same `parse_kv` as every other arg. It remains **reserved by meaning** (the
server/runner still read the `base` slot to know what to run), but **not by
syntax** — there is no bespoke positional/`image` codepath left.

- **Rename everywhere, including the internal tree entry.** The reserved ArgTree
  entry `image` → `base`. This **unifies `run` with `curry`**, which already
  calls its slot `base`. It **re-keys every cached result and the whole seed**
  (a one-time break): `build-builtins.sh`, the seed records/sentinels, the
  worker self-recursion (`/cas/args/image` → `/cas/args/base`), and the server
  reads all move together.

- **CLI grammar collapse (no sugar).** The positional `<image>` is **gone**.
  ```
  # before
  caos-cli run <image> [output] -- --a=b
  # after
  caos-cli run [output] -- --base:@@=<ref> --a=b
  ```
  `<output>` (a host result path, not an arg) stays.

- **`.caos-expr` grammar collapse.** The positional image and the `--` separator
  both dissolve; `base` is just one of the args.
  ```
  # before
  run <image> -- --in:@=.
  # after
  run --base:@=DEEP-DEPS/flake-builder --in:@=.
  ```
  This touches **every** `.caos-expr` in `std/` and the repo root. The
  `docker://seeded-*` sentinels become `--base:docker=seeded-*`.

- **Other image positions get the same treatment (stop sniffing everywhere).**
  map-then's `--map` / `--run` / `--then` become typed
  (`--map:docker=…`, `--map:@=…`, `--map:hash=…`) instead of sniffing their
  values.

## The consumer root `.caos-expr` (shape, for reference)

A non-nix consumer pins caos directly and needs **no expander at all**:

```
CAOS=run --base:@@=git+https://github.com/org/caos?rev=<sha>&dir=. -- ...
# graft CAOS in at flake-inputs/caos, then run its deep-deps over the merged tree
```

(Unavoidably two-step: you cannot `run std/deep-deps` until caos is mounted, and
mounting is what the expression does — deep-deps comes *out of* the mounted
`$CAOS`. A graft/merge worker plus `std/merge` assembles
`consumer-tree + flake-inputs/caos = caos-tree`.)

The mount lives only in the evaluation result — content-addressed, deduped,
never committed. `flake-inputs/` should be **gitignored** in the consumer.

**Pinning from `flake.lock` is lockfile codegen, not runtime magic.** A worker
can parse `flake.lock` (pure) but cannot fetch (network is client-only), and the
eval grammar's `$VAR` is an object reference, not a string you can splice into a
URL. So "keep the caos pin in sync with `flake.lock`" is a **dev/build hook**
that writes the resolved `git+https://…?rev=<sha>` locator into a tracked
generated file when you `nix flake update`. This keeps nix entirely out of core
*and* out of the evaluator. (Not in scope for the first cut; noted so the syntax
we pick — nix's — is the one `flake.lock` already emits, making that codegen a
mechanical mapping.)

## Implementation stages

1. **Unify the parser.** One `ArgType` enum + shared `parse_kv`, used by both
   `lib.rs` and `eval.rs`. No new behavior; closes the drift (and finally lets
   `.caos-expr` express `commit`/`hash`). Safe, prerequisite.
2. **`--base` rename + grammar collapse.** Reserved entry `image` → `base`;
   `run`/`curry`/worker-self-ref/server reads; delete shape-sniffing in
   `resolve_expr_base` / `run_request` / `resolve_run_image`; drop the CLI
   positional and the `.caos-expr` positional + `--`. Type the map-then image
   positions.
3. **New types.** `:docker=`, `:hash=` (generalize `:tree=`), and `:@@=` ref
   parse (`Ref { url, rev, dir }` + validation: mandatory `rev` for remote,
   reject `ref=`-without-`rev`).
4. **Locator fetch.** Client-side `git fetch --depth 1 <url> <rev>` → peel →
   descend `dir` → ingest → oid; local refs via the existing git-tracked ingest.
5. **Migrate std + bootstrap.** `docker://seeded-*` → `--base:docker=`;
   regenerate seed keys; update `build-builtins.sh` and every `.caos-expr`.
6. **Docs + tests.** SPEC.md, README, `design/caos-expr.md`, `design/commits.md`;
   `tests/eval-path` coverage for each new type; a remote-`:@@=` test.

Stage 1 is safe and prerequisite regardless of review outcome.

## Open / deferred

- The **consumer graft/merge worker** and the **`flake.lock` codegen hook** are
  sketched, not scheduled — they sit on top of this primitive and nothing in
  this repo needs them yet.
- A **worker-side** fetch (network in a container) is explicitly **out of
  scope**; if ever wanted it is a distinct, explicit grant, not something the
  general locator smuggles in.
