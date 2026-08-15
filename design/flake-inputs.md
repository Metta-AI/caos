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
| 3 | `:@@=` remote git-ref *parse* (`GitRef{url,rev,dir}` + validation) | ✅ done (33/33), **no redeploy needed** |
| 2C | CLI + worker `caos` grammar collapse: drop the positional image (and the `--`) everywhere; type the map-then positions; delete the last sniffers | ✅ done (33/33), **needed a redeploy** |
| 4 | `:@@=` *resolution* (client-side fetch → oid) | ✅ done (34/34), **no redeploy needed** |
| 4a | `dir=` descends through EVALUATION, not a raw tree walk — the fix that makes an ordinary `std/<x>` reachable by locator | ✅ done (35/35) |
| 6 | Docs + tests (README, `tests/remote-ref`) | ✅ done with 4 |

The end goal past stage 4: a consumer repo pins caos with a
`git+https://…caos?rev=<sha>` locator and reaches `std/*` by descent — nothing
committed, all content-addressed. The one-worker case works today
(`tests/remote-ref`); the whole-tree case needs one ordinary worker, not new
machinery. See **Consumer root** below.

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
`resolve_expr_args`). **Stage 1 already unified them**; the enum is now
`ArgType { Literal, Path, Remote, Commit, Hash, Docker }` — `Hash` the rename
of/over stage 1's `Tree`, `Docker` and `Hash` added by 2B, `Remote` (`:@@=`) by
stage 4.

| form | meaning | representation in the arg tree | status |
|---|---|---|---|
| `--n=v` | literal string | blob | done |
| `--n:@=<path>` | a path into the **ambient** git repo, relative to the `.caos-expr` dir — **unchanged**, always a bare path, never a scheme | tree/blob oid | done |
| `--n:@@=<ref>` | a **proper nix-style ref** to a tree in another repo (or local) | tree/blob oid (resolved at eval time) | done |
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
  `docker://`-prefix checks — `resolve_cas_image` reading a CAS file's content,
  `base_arg_entry`, the server's re-derivation — are untouched. What dies is
  guessing a bare token's type by shape. Resolving a ref caos itself RECORDED is
  not sniffing; guessing at a token a user typed is.
- **`:hash=` generalizes the existing `:tree=`** to a blob or tree by oid (verify
  it exists). `:commit=` stays separate (the *unpeeled* form).

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

## Stage 4: `:@@=` resolution — ✅ DONE

Client-side only, so it landed with **no redeploy**; 34/34 with a new
`tests/remote-ref`. `ArgType::Remote` + the `parse_arg` arm, and one resolver —
`resolve_remote_arg` — behind every `:@@=` position:

- **`Transport::fetch_git_ref(url, rev)`**, defaulting to `Ok(None)`. That
  default IS the security story, not an oversight: a worker cannot reach a
  network, so it gets "resolving a remote ref is a CLIENT capability" rather
  than a way to smuggle one in. By the time a worker sees a `:@@=` arg it is an
  ordinary oid. `tests/remote-ref/check.sh` asserts the refusal from inside a
  worker.
- **Pin, fetch, then select** — the order is the content-addressing argument.
  `git fetch --depth 1 <url> <rev>` (the granularity a host will serve), peel the
  commit to its tree, descend `dir=`. A `path:` skips the fetch and ingests a
  live local directory, exactly as `:@=` does.
- **Wired in three places, each behaving like its `:@=` sibling** — that is the
  whole rule, so where a tree came from never changes what naming it means:
  `build_arg_entries` (a plain arg → the oid), `resolve_base` (an image → the
  oid, then evaluated), and `resolve_expr_args`/`resolve_expr_base` in the
  evaluator (evaluated if it carries a `.caos-expr`, raw if not — the
  worker-vs-data rule, now factored out as `eval_if_evaluable` and shared).
- **A rev is a pin, so re-resolving is free.** `fetch_git_ref` returns early when
  the commit is already local; the test proves it by DELETING the source repo
  and resolving again.

### ⚠️ `git fetch` and a partial ALTERNATE object store

`git fetch` runs a connectivity check — `rev-list --not --all --alternate-refs`
— which walks the tips of every **alternate** object store as well. A repo whose
alternate holds a deliberate SUBSET then fails with `missing blob object <x>`
naming an object that has nothing to do with the fetch, blamed on the fetch.
The test harness creates exactly that shape (`tests/lib/run-test.sh` points the
client at `/tmp/seed-git/objects`, "exactly what this test declared"), so
`tests/remote-ref` failed the first time it ran — on a fetch that succeeds in
any ordinary repo. The difference is the alternate, not the command.

Fixed with `-c core.alternateRefsCommand=true` (a command that prints nothing)
on **this fetch only**. Alternate tips are an EXCLUSION set, so dropping them
makes the check verify our closure and only ours — stricter, not looser — which
is safe precisely because a `--depth 1` fetch is self-contained. Do NOT copy it
to `fetch_object_negotiated`: a chat commit's history may legitimately live in
an alternate, and there the tips are doing real work.

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

Deliberately left untouched at the time (isolated, hence low-risk): the
CLI/worker sniffers `resolve_run_image`/`resolve_cli_image`, so
`caos-cli run <dir>` and `caos curry <img>` still used positional grammar.
That was stage 2C, now done.

## Stage 3: the `:@@=` locator parser — ✅ DONE

Pure string logic, so it landed and was verified **in-session** (33/33) with **no
redeploy** and no stack fixture: `GitRef { url, rev: Option, dir: Option }` plus
`parse_git_ref` in `lib.rs`, with a `git_ref_tests` module covering each rule.
The validation *is* the feature — it is what makes a URL behave like content:

- a git scheme (`git+https://`, `git+ssh://`, `git+file://`, `github:`) **must**
  carry `rev=<40-hex>`; no rev is an error, and a short rev is an error;
- a `ref=` (branch/tag) is **rejected outright**, even alongside a `rev=` —
  mutable input never enters a cache key, and refusing the ambiguous both-form
  keeps there from being a "which won?" question;
- `path:` is a plain local directory: **no** rev (hashed live, like `:@=`);
- unknown scheme, unknown query key, a non-`key=value` query part and a repeated
  `rev=`/`dir=` are all errors — the grammar is closed, so a typo can't be
  silently ignored into a wrong-but-plausible fetch.

`GitRef`/`parse_git_ref` carry `#[allow(dead_code)]`: nothing *reads* the fields
until stage 4 wires resolution in, and `unit-clippy` runs `--all-targets -D
warnings`, so a helper exercised only by `#[cfg(test)]` still trips `dead_code`
in the non-test lib build. **Drop the allows in stage 4**, when the resolver
reads them.

Not yet done here (deliberately, they are stage 4): `parse_arg` does **not** yet
accept `:@@=` — the key split is `key.split_once(':')`, so the arm is a
one-liner (`Some((name, "@@"))`) whenever the resolver behind it exists.

## Stage 2C: CLI + worker `caos` grammar collapse — ✅ DONE

The other half of "no special arg": the positional image is gone from the CLI and
from the worker `caos` subcommands, the `map-then` image positions are typed, and
the last sniffers are deleted. Verified **33/33 after a redeploy** (`nix build
.#caosd && ./result-caosd/bin/caosd up`), which this stage genuinely needed —
`caos-tools/*.sh` and `std/flake-builder/worker` run on the deployed stack, so
their grammar and the deployed `caos` binary have to move together. It re-keys
nothing: the arg trees are byte-identical, so every seed record held (bootstrap's
own `caos-cli curry --base:hash=…` is the proof — the stack came up clean).

### The final grammar

```
caos-cli run [--trace…] [output] --base:<t>=<image> [--k=v …]
caos-cli curry [--unbind=<n> …] --base:<t>=<arg tree> [--k=v …]
caos     curry [--unbind=<n> …] --base:<t>=<arg tree> [--k=v …]
caos     map-then <in> [--map:<t>=<img>] [--then:<t>=<img>]
caos     run-then <in>  --run:<t>=<img> [--then:<t>=<img>] [--catch]
```

**There is no `--` anywhere** — this was the one open question, and collapsing it
is what makes the grammar one rule instead of four. What keeps a verb's own
operands apart from the args it binds is that their NAMES are reserved (`base`,
`unbind`), exactly as 2B did it for `.caos-expr`; a `--` separating regions was
the last remnant of positional thinking. Consequences worth knowing:

- `--unbind` may now sit anywhere among the binds, and an arg literally named
  `unbind` cannot be bound (it is reserved, like `base`).
- The CLI's `[output]` stays positional — it is a HOST path, not an arg — and is
  identified as "the token that isn't a `--flag`". Sound because every argument
  in the grammar is `--name[:type]=value`.
- `<in>` on `map-then`/`run-then` stays positional too: it is the DATA the
  continuation is over, not an image.

### What changed

- **`split_base_arg`** (`lib.rs`) pulls the reserved `--base` out of any verb's kv
  list (exactly one required), and **`resolve_base`** resolves a typed image ref —
  the single function every image position now goes through: `--base` on both
  clients, and `--map`/`--run`/`--then` in `record_continuation`.
- **The sniffers are gone.** `resolve_run_image` split into `resolve_cas_image`
  (worker, `:@=` only) and `resolve_cli_image` (CLI, `:@=` only — a host dir to
  ingest+evaluate, now an ERROR if it isn't a directory). Its `docker://`-prefix
  and hex-hash branches became the `:docker=`/`:hash=` types. What *stays* is
  reading a CAS file's CONTENT for a `docker://` ref: the path was typed `:@=` by
  the operator and what's found there is an object caos recorded — resolving a
  stored ref, not guessing at a token. Same reason the server's re-derivation
  (`compute.rs` sub-request builder) and `base_arg_entry` are untouched.
- **`worker_common::Arg`** gained `Hash` and `Docker` (and `#[derive(Clone,
  Copy)]`), and `caos_curry`/`caos_recurry`/`map_then`/`run_then`/
  `run_then_catching` now take `Arg` for every image instead of `&str`. This is
  where the change earned its keep: each Rust worker had to *say* what it was
  holding, and the answers were all already knowable — `own_image()` is a
  `/cas` PATH, `own_args_tree()`/`caos_curry`'s output are HASHES, and an image
  read out of an arg (`llm-step`'s `image_arg`, `worker-rustc`'s
  `read_arg("runner")`) is a hash literal. `tests/file-count/worker.rs` was the
  one place the two kinds were genuinely mixed behind one `String`
  (`recur_arg_tree` returned a curry hash OR the bare image path); it is now
  typed at each branch, which is strictly clearer than what it replaced.

### Migrated in lockstep

`build-builtins.sh` (both seed curries), `std/flake-builder/worker`,
`caos-tools/build.sh` + `test.sh`, ~85 `tests/*/cli.sh` sites, the
`tests/run-then/*.sh` worker fixtures, `tests/{commit,file-count}`'s Rust worker
fixtures, `examples/consumer`, README/SPEC/`design/{map-then,caos-expr,
flake-images,cargo-workers}.md`, plus every stale in-code comment naming the old
forms.

**One migration trap worth repeating:** a mechanical `run "$x" … --` →
`--base:hash="$x"` sweep is wrong wherever `$x` holds a PATH. `tests/bash-tool`
sets `tool=DEEP-DEPS/bash-tool` and went red with `:hash= wants an object hash`.
The sweep can't know; check what each variable actually holds.

### ⚠️ The trap that already bit us (2A) — still relevant

**`base` collides with the git-docker image tree.** An image tree carries a
`base` entry of its own — the `docker://` ref its `layer<NN>`s delta over (SPEC,
"Git-tree image"). 2A's `args_tree_node` used "has a `base` entry ⇒ args tree"
and peeled *every image tree* (fixed by returning `None` for a tree carrying
`config.json`). **Anywhere later stages inspect tree entries by the `base` name
must keep image-trees vs args-trees apart the same way.**

---

## Consumer root `.caos-expr` (the payoff)

The SIMPLE half of this now works and is tested. A consumer entry that only
needs one caos worker names it by locator and curries its own script on —
`tests/remote-ref` runs exactly this, with a synthetic consumer repo:

```
curry --base:@@=git+https://github.com/org/caos?rev=<sha>&dir=std/bash --worker1:@=run.sh
```

Nothing of caos is committed in the consumer; the pin is the whole dependency,
and the ArgTree carries caos' oids, not its URL.

### `dir=` descends through EVALUATION (fixed after stage 4)

Stage 4 shipped `dir=` as a **raw** tree walk (`lookup_in_tree`), evaluating only
the final subtree. That looked like a hard constraint on the consumer story and
was not — it was a bug, and it made three quarters of `std/*` unreachable by
locator:

```
--base:@@=…&dir=std/rgrep  →  base path "DEEP-DEPS/rustc" not found in tree
--base:@@=…&dir=std/rustc  →  seeded sentinel docker://seeded-rustc … cannot be answered
```

Both symptoms are the same mistake. A raw walk hands the evaluator a bare
`std/<x>` directory, but an entry's expression names `DEEP-DEPS/<dep>` mounts
that exist only after the ROOT expression has deepened the tree — and a seeded
entry like `std/rustc` forms its key from its *deepened* entry, which a raw
fetch cannot reproduce. Nothing about the model required this; `eval-path`
already descends through evaluation everywhere else, which is exactly how
`DEEP-DEPS/<x>` resolves inside this repo.

`resolve_remote_arg` now descends with `eval::eval_path` from the fetched root,
so a locator names a path in the **evaluated** tree. Measured after the change,
same probe:

| `--base:@@=…&dir=<entry>` | before | after |
|---|---|---|
| `std/flake-builder`, `std/deep-deps` | ✅ | ✅ |
| `std/rustc` | ❌ | ✅ |
| `std/bash` | ❌ | ✅ |

(`std/rgrep` then fails only because this stack's *seeded* rustc carries a
pre-2C `worker-common`, so rgrep's post-2C call sites do not compile against it
— stack staleness a `caosd up` clears, not a locator limit.)

**A pinned consumer therefore sees caos exactly as caos sees itself**, and an
ordinary worker — one with a `.caos-expr` and `DEPS`, built by rustc — is
reachable. That is what makes the design below possible; the earlier sketch of a
graft worker that had to be a flake was an artifact of the bug.

What remains is the WHOLE-TREE case — a consumer that wants `DEPS`/`DEEP-DEPS`
of its own, so its directories can declare deps against caos' `std/*`. Now that
a locator reaches an ordinary entry, this is **one line and one new worker**:

```
run --base:@@=git+https://github.com/org/caos?rev=<sha>&dir=std/<expander> \
    --in:@=. --caos:@@=git+https://github.com/org/caos?rev=<sha> --expr=$CAOS_EXPR
```

The locator appears TWICE, and that is not redundancy to design away — the two
name different things. `--base` yields the expander's IMAGE; `--caos` yields
caos' TREE, which is what gets mounted. A worker cannot fetch, so the tree has
to arrive as an already-resolved arg. The duplication is exactly what the
consistency check below exists to police.

The expander is a normal caos worker — a `.caos-expr`, `DEPS`, built by rustc —
because resolving that locator evaluates caos from its root, so deep-deps and
rustc run for the consumer exactly as they do here. There is no bootstrap
problem to solve and no `std/merge` step: the consumer already has a caos server
with a seeder and a runner, which is the only precondition.

What it does:

- read `--in`'s `flake.nix` / `flake.lock` and determine which caos revision the
  consumer pins;
- read the expression that launched it — `--expr=$CAOS_EXPR` — and CHECK that
  both locators on that line name the same repo and revision as `flake.lock`, so
  a tree cannot be evaluated against one caos while its lockfile declares
  another, and the two locators cannot drift apart;
- return `--in` with the `--caos` tree mounted (say `.caos-input/`, or just its
  `std/` as `.caos-std/` — naming still open).

Everything that check needs arrives as an arg: `flake.lock` in `--in`, the tree
in `--caos`, the declaration in `--expr`. Nothing is fetched, nothing is
ambient. `--expr=$CAOS_EXPR` is required because the directive is STRIPPED from
`--in:@=.` — `--expr:@=.caos-expr` names a file that is not there (measured:
`eval-path: path ".caos-expr" not found in tree`), which is what `$CAOS_EXPR`
exists for.

One limit worth stating rather than discovering later: **no worker can verify
that a tree came from a rev**, because that mapping needs a fetch. So this check
compares DECLARATIONS — the locators against the lockfile — and is a drift
detector, not a proof about `--caos`'s contents.

The mount lives only in the evaluation result — content-addressed, deduped,
never committed; the mount point is **gitignored** in the consumer.

It does not need to run deep-deps itself: the mounted result is an ordinary
tree, so the consumer's root expression chains to caos' deep-deps in a second
line if it wants `DEPS` resolution too.

**Pinning from `flake.lock` is lockfile codegen, not runtime magic.** A worker
can parse `flake.lock` (pure) but cannot fetch (network is client-only), and the
eval grammar's `$VAR` is an object reference, not a string you can splice into a
URL. So "keep the caos pin in sync with `flake.lock`" is a **dev/build hook**
that writes the resolved `git+https://…?rev=<sha>` locator into a tracked
generated file on `nix flake update`. Keeps nix out of core *and* the evaluator.
The syntax we chose (nix's) is the one `flake.lock` already emits, so that codegen
is a mechanical mapping. Not scheduled; noted so the choices line up.

## Open / deferred

Stages 1–4 are done; the grammar and the locator are finished. What is left is
built ON them, and nothing in this repo needs it yet:

- The **consumer input expander** (whole-tree mounting, above) and the
  **`flake.lock` codegen hook** — sketched, not scheduled.
- A **worker-side** fetch (network in a container) is explicitly **out of
  scope**; if ever wanted it is a distinct, explicit grant, never something the
  general locator smuggles in. The `Ok(None)` default on
  `Transport::fetch_git_ref` is what holds that line, and `tests/remote-ref`
  asserts it from inside a worker.
- **`ref=` (a branch/tag) stays refused**, with no plan to relax it. Anyone who
  wants "track main" wants a lockfile, which is the codegen hook above — the
  refusal is the invariant, not a missing feature.
- **`map-then`/`run-then` positional `<in>`**: resolved in 2C — it stays
  positional. It is the data the continuation is over, not a base.
