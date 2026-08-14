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
| **2B** | **Grammar collapse: `run`/`curry` take `--base`; drop positional image + `--`; type map-then positions; add `:docker=`/`:hash=`** | **← NEXT, not started** |
| 3 | `:@@=` remote git-ref *parse* (`Ref{url,rev,dir}` + validation) | not started |
| 4 | `:@@=` *resolution* (client-side fetch → oid) | not started |
| 5 | Migrate every `.caos-expr` + `build-builtins.sh` to the new grammar | folded into 2B |
| 6 | Docs + tests (SPEC, README, `tests/eval-path`, a remote-ref test) | ongoing |

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

## Stage 2B: `--base` full collapse (next up) — detailed

"The thing we run" becomes an **ordinary typed arg** named `base`, parsed by the
same `parse_arg` as everything else. It stays **reserved by meaning** (server and
runner read the `base` slot to know what to run — 2A already made that so), but
**not by syntax** — no bespoke positional/`--` codepath survives.

### The one semantic subtlety

A base's value needs **image resolution**, not plain arg resolution: a
`:@=DEEP-DEPS/flake-builder` base must resolve *through that subtree's
`.caos-expr` to the image it builds* (today's `resolve_expr_image`), whereas a
plain data arg's `:@=` references the tree. In practice these converge for the
directory case (an evaluable dir evaluates; a plain dir is identity), and the
docker/hash/var shapes move to explicit types. Recommended shape:

- Keep resolving the **base to a ref *string*** (so `assemble_arg_tree(image:
  &str, …)` / `base_arg_entry` are unchanged), by dispatching on the base arg's
  `ArgType`:
  - `:@=path` → look up path, eval-path → hash string (today's
    `resolve_expr_image` path branch, kept — just reached via the typed arg
    instead of the positional token).
  - `:docker=X` → `"docker://X"`.
  - `:hash=X` → `X` (validate).
  - `$VAR` → the variable's oid string.
  - `:@@=ref` → stage 4 (fetch → hash string).
- Resolve the **remaining args** with `resolve_expr_args` as today.
- `assemble_arg_tree` / `curry_from_entries` fold the base string into the
  reserved `base` entry (unchanged from 2A).

### Front-end sites to change (pointers)

- **`.caos-expr` evaluator:** `crates/caos/src/eval.rs`
  - `eval_command` (~L283): currently requires `--` at token position 2 and
    treats `tokens[1]` as the image. New: no positional; scan the arg tokens for
    the one whose name is `base`, resolve it as the base (above), resolve the
    rest as args. Update the module grammar doc block at the top (~L12–21) and
    the per-fn docs.
  - `resolve_expr_image` (~L328, the `$`/`docker://`/hex/path sniffer):
    becomes the base-string resolver dispatched on `ArgType` (see above). Delete
    the shape-sniffing branches; each shape now arrives as an explicit type.
- **CLI `run`/`curry`:** `crates/caos/src/lib.rs`
  - `run_request` (~L2302), `prepare_request` (~L2328): drop the positional
    `image: &str`; pull `--base` out of the kvs. `<output>` (a host result path,
    NOT an arg) stays. Argv parsing lives in `crates/caos/src/bin/caos-cli.rs`.
  - `resolve_run_image` (worker/CLI image sniffer): the `--map`/`--run`/`--then`
    and worker-`caos curry` image args route through it. Type them; delete the
    sniffing. This is the map-then image-position parser touched in stage 1
    (lib.rs ~L2490 region).
- **Worker `caos` subcommands:** `crates/caos/src/bin/caos.rs` — `caos curry`,
  `caos run-then`, `caos map-then`. `curry` collapses like the others. **Open
  detail:** `map-then`/`run-then` take a *positional `<in>`* (the data node,
  single-assignment at `/cas/out`) that is NOT a base — decide whether `<in>`
  stays positional (likely yes; only `--map`/`--run`/`--then` get typed) and
  whether the `--` separator stays for it. Note in `design/map-then.md`.
- **Server:** `crates/server/src/compute.rs` sub-request builder (~L734) has
  `image.len()==40 && hex → tree entry, else blob`. **Keep it** — that is
  re-deriving the entry representation from a stored ref (the server's
  `base_arg_entry`), NOT user-value sniffing. Claude confirmed the server's
  `unwrap_curry` only peels `CURRY_MARKER`, so it was never confused by `base`.

### `.caos-expr` migration (all 14 files) — uniform rewrite

Every current expr is `run <IMG> -- --in:@=.` (or a `curry <IMG> -- …`). Rewrite
to `run --base:<TYPE>=<IMG> --in:@=.`:

| `<IMG>` today | becomes |
|---|---|
| `docker://seeded-X` (sentinels: flake-builder=`seeded`, and `seeded-{rustc,deep-deps,runner}`) | `--base:docker=seeded-X` |
| a path (`std/deep-deps`, `deep-deps`, `DEEP-DEPS/flake-builder`, …) | `--base:@=<path>` |

Files: `./.caos-expr`, `./std/.caos-expr`, and `std/*/.caos-expr` for
`bash bash-tool cargo deep-deps flake-builder llm-call llm-step llm-stub merge
rgrep runner rustc`. (rustc/deep-deps/runner/flake-builder are the docker
sentinels; the rest are paths.)

### Tool scripts + bootstrap to migrate

- `std/flake-builder/worker`: `caos run-then /cas/args/in -- --run="$build" --then="$stack"`
  → typed image args. Self-recursion `caos curry /cas/args/base -- …` →
  `caos curry --base:@=/cas/args/base …` (worker-side `:@=` is a `/cas` path).
- `caos-tools/build.sh`, `caos-tools/test.sh`: several `caos run-then … --run=…`
  / `caos map-then … --map=… --then=…` (e.g. `--run=docker://seeded` →
  `--run:docker=seeded`; `--run=/cas/args/result` → `--run:@=/cas/args/result`;
  `--map="$var"`/`--then="$var"` where the var holds a hash → `:hash=`) and the
  `caos curry /cas/args/base -- …` self-recursions.
- `build-builtins.sh`: the seed `required` maps already emit `{"base":…,"in":…}`
  (2A). The `docker://seeded*` blobs it hashes (`printf 'docker://seeded…'`) stay
  as-is — they are the stored representation `:docker=` also produces, so the
  formed key still matches. **Only the `.caos-expr` tokens change**, not the seed
  bytes. Verify key-match holds after migration (a mismatch shows up as a seeded
  item falling through to the generic runner and dying on the sentinel image).

### ⚠️ The trap that already bit us (2A) — keep it in mind for 2B

**`base` collides with the git-docker image tree.** An image tree carries a
`base` entry of its own — the `docker://` ref its `layer<NN>`s delta over (SPEC,
"Git-tree image"). 2A's `args_tree_node` used "has a `base` entry ⇒ args tree"
and so peeled *every image tree*, scattering `config.json`/`layer<NN>` into the
args and sending runs to the raw server-side registry name
(`lookup caos-registry … no such host`). Fixed by making `args_tree_node` return
`None` for a tree carrying `config.json` (the converter requires it; a `.`-name
can't be an arg). **Anywhere 2B inspects tree entries by the `base` name must
keep image-trees vs args-trees apart the same way.**

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
