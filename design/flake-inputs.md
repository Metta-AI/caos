# Flake inputs, `--base`, typed arg locators, and remote git refs

**What this is:** the design + rolling implementation log for making caos usable
as a dependency of *another* project (the "consumer story"), by way of a
uniform, typed argument grammar. Read the **Status** and **Working here**
sections first — they tell a fresh session exactly where to pick up and which
traps have already been sprung.

---

## Status (read this first)

| stage | what | state |
|---|---|---|
| 1 | Unify the two arg parsers into one `ArgType` + `parse_arg` | ✅ done, merged |
| 2A | Rename the reserved ArgTree entry `image` → `base` (wire/disk/seed) | ✅ done (33/33), merged |
| 2B | `.caos-expr` grammar collapse: `run`/`curry` take `--base`, no positional/`--`; add `:docker=`/`:hash=`; migrate all `.caos-expr` | ✅ done (33/33), **no redeploy needed** |
| **2C** | **CLI + worker `caos` grammar collapse: drop the positional image everywhere; type the map-then positions; delete the last sniffers** | **← NEXT, not started** |
| 3 | `:@@=` remote git-ref *parse* (`Ref{url,rev,dir}` + validation) | not started |
| 4 | `:@@=` *resolution* (client-side fetch → oid) | not started |
| 6 | Docs + tests (SPEC, README, a remote-ref test) | ongoing |

The end goal past stage 4: a consumer repo pins caos with one
`--base:@@=git+https://…caos?rev=<sha>` locator, grafts it in at
`flake-inputs/caos`, and reaches `std/*` by descent — nothing committed, all
content-addressed. See **Consumer root** below.

## Working here (hard-won workflow notes)

- **Build/test through the tools, not host cargo.** There is no host toolchain.
  `run-tool build` compiles the sources inside caos; `run-tool test` runs the
  33-test suite. An unchanged test is a cache hit.
- **Protocol changes cannot be verified in-session without a redeploy.** The
  `build`/`test` tooling runs on the *deployed* outer stack (`caos-tools/*.sh`
  execute there). Anything that changes the wire/disk contract — the reserved
  entry name, `/cas/args/*` paths, the runner `required` map, seed keys, or the
  `run`/`curry` grammar the tool scripts themselves use — only takes effect
  after `nix build && ./result/bin/caosd up`. Until then `build` dies early
  (e.g. `caos curry /cas/args/base` against a stack that still writes
  `/cas/args/image`). **2B is such a change** (the tool scripts use `caos
  curry`/`run-then`): expect to make the edits, hand off for a redeploy, *then*
  test. This is the same shape as the socket-grant merge and the 2A rename.
- **fmt is a graded test.** `unit-fmt` fails the suite on any rustfmt diff. A
  rename that shortens a literal can make rustfmt want a line collapsed — run
  fmt before declaring green (2A's 33rd failure was exactly this).
- **Stale redis seed-results.** Results are keyed by arg-tree hash, so most go
  stale on their own when an upstream hash moves — but **seed keys are stable**,
  so a seeded sentinel's memoized result can keep serving a now-wrong value
  after you change how its key is formed. 2A had to drop one
  `caos:result:<hash>` by hand. If a seeded item misbehaves after a key change,
  suspect this.

---

## The two problems this solves

1. **The consumer story.** A project that uses caos (as a nix flake input) must
   reach caos' `std/*` from its own root `.caos-expr`, which means the caos tree
   has to be *mounted into the project's tree* — but only into the **evaluation
   result** (the "expression's tree"), never committed. Today there is no way to
   name a tree that lives in another repo.

2. **Sniffing.** Image position (in `.caos-expr` and on the CLI) guesses what a
   bare token means by its *shape* — `docker://…` vs a hex hash vs a path. That
   violates the codebase's own rule ("the type is chosen by the operator, not by
   sniffing the value") that every *arg* already follows.

One mechanism fixes both: **make "the thing we run" an ordinary typed arg
(`--base`)**, and **add a typed locator for a tree in another repo (`:@@=`)**
whose pin is a content hash, so it caches like everything else.

## Core principle (holds for all remaining stages)

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

One parser, one `ArgType` enum, shared by the CLI (`crates/caos/src/lib.rs`,
`parse_arg`) and the `.caos-expr` evaluator (`crates/caos/src/eval.rs`,
`resolve_expr_args`). **Stage 1 already unified them** — the enum is
`ArgType { Literal, Path, Commit, Tree }` today; 2B/3 add `Docker`, `Hash`
(rename of/over `Tree`), and `Remote` (`:@@=`).

| form | meaning | representation in the arg tree | status |
|---|---|---|---|
| `--n=v` | literal string | blob | done |
| `--n:@=<path>` | a path into the **ambient** git repo, relative to the `.caos-expr` dir — **unchanged**, always a bare path, never a scheme | tree/blob oid | done |
| `--n:@@=<ref>` | a **proper nix-style ref** to a tree in another repo (or local) | tree/blob oid (resolved at eval time) | stage 3/4 |
| `--n:hash=<oid>` | an existing object by hash (generalizes today's `:tree=` to blob/tree) | that oid | 2B |
| `--n:commit=<rev>` | unpeeled commit — **unchanged** | gitlink (mode 160000) | done |
| `--n:docker=<ref>` | a docker image ref | blob `docker://<ref>` | 2B |
| `--n=$VAR` | (eval only) an object a prior line produced | that oid | done |

Notes:
- **`:@=` vs `:@@=` — two operators, deliberately.** `:@=` stays a bare path
  (the common case; no `path:` prefix to type). `:@@=` takes the full ref
  grammar. Local refs *can* be `:@@=path:./x`, but you'd rarely bother.
- **`:docker=` / "stop sniffing" is a parse-time change only.** `:docker=alpine`
  still *stores* the blob `docker://alpine`, so the downstream
  `docker://`-prefix checks (`resolve_run_image`, the server's re-derivation)
  are untouched. What dies is guessing a bare token's type by shape.
- **`:hash=` generalizes the existing `:tree=`** to a blob or tree by oid (verify
  it exists). `:commit=` stays separate (the *unpeeled* form).

## `:@@=` locator grammar (nix-style) — stage 3/4

Canonical:

```
git+https://github.com/company/repo?rev=<40-hex>&dir=std/deep-deps
git+ssh://git@github.com/company/repo?rev=<40-hex>&dir=x
path:./some/dir            # local, no rev
git+file:///abs/repo?rev=<40-hex>&dir=x
```

Optional client-normalized sugar: `github:owner/repo?rev=<sha>&dir=x`.

Rules:
- **`rev` mandatory for remote schemes**, a **full-length hex commit sha**. A
  `ref=` (branch/tag) without `rev` is an **error** — that rejection is what
  enforces the content-addressing invariant.
- **`rev` absent for local schemes** — local content is hashed live at eval time,
  honoring "only what git tracks is visible."
- `dir=` selects a subtree within the repo.

Why commit-then-path (not a bare subtree hash): **GitHub only serves *commits***
(`allowReachableSHA1InWant`), so `git fetch --depth 1 <url> <rev>` is the
fetchable granularity; `dir` selects within it. The commit is also the pin.

Resolution (stage 4, client, eval time): `git fetch --depth 1 <url> <rev>` →
verify rev → peel → descend `dir` → ingest closure → **the oid is the arg
entry**, byte-for-byte as if from a local `:@=`. URL/rev are fetch coordinates
and provenance (trace), never part of the key.

Parser safety (verified against current code): the token is whitespace-free;
`split_once('=')` takes the **first** `=`, so `?rev=…&dir=…` survives in the
value; the `:`-type split runs on the **key** only, so `://` is untouched; and
`.caos-expr` comments are **full-line only** (`eval.rs` trims then checks
`starts_with('#')`), so a value may contain anything.

---

## Stage 2B: `.caos-expr` grammar collapse — ✅ DONE

The `.caos-expr` surface no longer has a positional image or a `--`. A `run`/
`curry` is `verb --base:<type>=<image> [--k…]`; the reserved `--base` names the
worker, typed like any arg. Landed **without a redeploy** — the collapse produces
byte-identical arg trees (same `base`+args entries), so every seed key held and
the outer stack kept answering. What changed:

- **`parse_arg`/`ArgType`** (`lib.rs`): `:tree=`→`:hash=` (generalized to a tree
  *or* blob by oid) and new `:docker=` (stores blob `docker://<ref>`). Wired in
  `build_arg_entries` (CLI/worker args) and the evaluator.
- **Evaluator** (`eval.rs`): `eval_command` scans tokens for `--base` (no
  positional, no `--`); `resolve_expr_image` → `resolve_expr_base`, dispatched on
  the explicit `ArgType` (docker/hash/path/`$VAR`) — **no sniffing here anymore**.
  `resolve_expr_args` handles `:docker=`/`:hash=`. Module grammar doc updated.
- **Migrated every `.caos-expr`**: all 14 committed (`run <IMG> -- …` →
  `run --base:@=<path>|:docker=<sentinel> …`, incl. the multi-line `llm-step`
  curry/variable form), **plus the runtime-generated fixtures** in
  `tests/{eval-path,secrets,deep-deps}/cli.sh` (this was the one thing missed on
  the first pass — those write `.caos-expr` at test time in old grammar and the
  new evaluator rejects them: `argument must look like --name=value, got: bash`).

Deliberately left untouched (isolated, hence low-risk): the CLI/worker sniffers
`resolve_run_image`/`resolve_cli_image`, so `caos-cli run <dir>` and
`caos curry <img>` still use positional grammar. That is stage 2C.

## Stage 2C: CLI + worker `caos` grammar collapse — ← NEXT

The other half of "no special arg": drop the positional image on the CLI and in
the worker `caos` subcommands, type the map-then image positions, delete the last
sniffers. **This one DOES touch the tool scripts and test `cli.sh`, so it re-keys
nothing but changes the grammar those scripts use — expect a redeploy** (they run
on the outer stack), unlike 2B.

Pointers (verified this session):

- **CLI `run`/`curry`:** argv parsing in `crates/caos/src/bin/caos-cli.rs`
  (`Some("run")` ~L43 matches `[image, output?, "--", kvs…]`; `Some("curry")`
  ~L109 passes `arg_tree` positional). Drop the positional; pull `--base` from
  kvs. Keep the CLI `--` separating the optional `output` (a host result path,
  NOT an arg) from the args. `cli_run`/`run_request`/`prepare_request`
  (`lib.rs` ~L2302/2328) currently take `image: &str` — feed it from the parsed
  `--base` instead. **CLI `:@=` base** = a host dir → `resolve_cli_image`
  (ingest+eval); make it path-only (docker/hash now arrive as their own types).
- **Worker `caos`:** `crates/caos/src/bin/caos.rs` — `curry` (drop positional
  base → `--base`), `run-then`/`map-then` (keep the positional `<in>` data node;
  type `--map`/`--run`/`--then`). The map-then image-arg parser is in `lib.rs`
  (~L2560 region, matches `ArgType`, calls `resolve_run_image`). `resolve_run_image`
  (`lib.rs` ~L2723) is the worker/CLI sniffer: split its `docker://`/hex branches
  out to the `:docker=`/`:hash=` types, leaving it to resolve only a `:@=`
  `/cas` path (whose *content* may still be a `docker://` blob — that's resolving
  a recorded object, not sniffing a token, and stays).
- **Tool scripts + fixtures to migrate with 2C** (they use worker/CLI grammar):
  `std/flake-builder/worker` (`caos run-then … --run=… --then=…`, `caos curry
  /cas/args/base …`), `caos-tools/build.sh` + `test.sh` (`--run=docker://seeded`
  → `--run:docker=seeded`; `--run=/cas/args/result` → `--run:@=…`; `--map/--then`
  `$var` holding a hash → `:hash=`; `caos curry /cas/args/base …`), and the many
  `tests/*/cli.sh` doing `caos-cli run DEEP-DEPS/x …` / `curry DEEP-DEPS/x --`.
- **Server:** `crates/server/src/compute.rs` sub-request builder (~L734)
  `image.len()==40 && hex → tree entry, else blob`. **Keep** — that re-derives
  the entry from a stored ref (the server's `base_arg_entry`), not user-value
  sniffing.
- **Open detail:** whether `map-then`/`run-then` keep the `--` before their typed
  image flags (the `<in>` stays positional regardless). Note in
  `design/map-then.md`.

### ⚠️ The trap that already bit us (2A) — still relevant

**`base` collides with the git-docker image tree.** An image tree carries a
`base` entry of its own — the `docker://` ref its `layer<NN>`s delta over (SPEC,
"Git-tree image"). 2A's `args_tree_node` used "has a `base` entry ⇒ args tree"
and peeled *every image tree* (fixed by returning `None` for a tree carrying
`config.json`). **Anywhere later stages inspect tree entries by the `base` name
must keep image-trees vs args-trees apart the same way.**

---

## Consumer root `.caos-expr` (the payoff, stage 4+)

A non-nix consumer pins caos directly and needs **no expander at all**:

```
CAOS=run --base:@@=git+https://github.com/org/caos?rev=<sha>&dir=. -- ...
# graft CAOS in at flake-inputs/caos, then run its deep-deps over the merged tree
```

Unavoidably two-step: you cannot `run std/deep-deps` until caos is mounted, and
mounting is what the expression does — deep-deps comes *out of* the mounted
`$CAOS`. A graft/merge worker (plus `std/merge`) assembles
`consumer-tree + flake-inputs/caos = caos-tree`. The mount lives only in the
evaluation result — content-addressed, deduped, never committed;
`flake-inputs/` is **gitignored** in the consumer.

**Pinning from `flake.lock` is lockfile codegen, not runtime magic.** A worker
can parse `flake.lock` (pure) but cannot fetch (network is client-only), and the
eval grammar's `$VAR` is an object reference, not a string you can splice into a
URL. So "keep the caos pin in sync with `flake.lock`" is a **dev/build hook**
that writes the resolved `git+https://…?rev=<sha>` locator into a tracked
generated file on `nix flake update`. Keeps nix out of core *and* the evaluator.
The syntax we chose (nix's) is the one `flake.lock` already emits, so that codegen
is a mechanical mapping. Not scheduled; noted so the choices line up.

## Open / deferred

- The **consumer graft/merge worker** and the **`flake.lock` codegen hook** are
  sketched, not scheduled — they sit on top of the `:@@=` primitive and nothing
  in this repo needs them yet.
- A **worker-side** fetch (network in a container) is explicitly **out of
  scope**; if ever wanted it is a distinct, explicit grant, never something the
  general locator smuggles in.
- **`map-then`/`run-then` positional `<in>`**: decide in 2B whether it collapses
  (probably not — it's data, not a base).
