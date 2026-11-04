# Wasm workers — WebAssembly in a V8 isolate, as a caos worker

**Status:** sketch (2026-W45-4). Nothing is built. This is the shape to argue
about before a spike.

## Goal

- A worker can be a `.wasm` module. A host worker runs it in a V8 isolate, and
  the module keeps the worker contract: args at `/cas/args`, result at
  `/cas/out`, and continuations for sub-work (`design/map-then.md`).
- What the guest can do:
  - read `/cas`, lazily and transparently, with no `caos get` dance and no
    `paths` list
  - read and write `/tmp`
  - call the caos worker verbs: `get`, `put`, `hash`, `curry`, `run-then`,
    `map-then`, `eval-path-then`, `run-request-then`, `sub-run`, …
  - read `/secret` where the tool is granted one
- Building the module is just a caos job: source goes in, a ready-to-run tool
  comes out, memoized end to end, the way `std/rustc` builds a binary today.

Why have it at all:

- **A narrower sandbox than a container.** A container worker has the network
  and can start processes (std/README, "Workers have the network"). A guest
  gets two preopened directories and an allowlist of caos verbs, and nothing
  else.
- **One warm image for every wasm tool.** Every wasm tool shares the same
  `base`, so any warm host takes any wasm job, as `std/runner` already does for
  compiled binaries.
- **Small artifacts that run on any CPU.** One `.wasm` runs on x86_64 and
  aarch64 runners, with no per-target build.
- **Many languages, one runtime:** Rust, Go, C/C++, Zig and AssemblyScript.

## Shape: a copy of runner and rustc

| today | wasm |
|---|---|
| `std/runner`: execs the `worker1` binary | `std/wasm`: instantiates the `module` blob in an isolate |
| `std/rustc`: `src` → `curry(runner, worker1=<bin>)` | `std/rust-wasm`: `src` → `curry(wasm, module=<x.wasm>)` |
| `worker-common` (the worker-side helper crate) | `caos-guest` (the guest-side crate) |
| `std/cargo`, baked for (musl, dev) | `std/cargo-wasm`, the same toolchain baked for `wasm32-wasip1` |

A tool is then exactly as small as `std/hello`:

```
# std/hello-wasm/DEPS
../rust-wasm rust-wasm

# std/hello-wasm/.caos-expr
HELP=<<END
Greet, and count the bytes of a README.
@param [name] Who to greet.
@in
END
IMAGE=run --base:@=DEEP-DEPS/rust-wasm --src:@=.
curry --base=$IMAGE --help=$HELP
```

A prebuilt module needs no factory:
`curry --base:@=DEEP-DEPS/wasm --module:@=tool.wasm`.

**New reserved arg names:**

- `module`: the wasm blob. It is not `worker1`, because `worker1` is the exec
  chain's slot and the module is never exec'd.
- `module-cache` (optional): see "Warmth".
- `wasm-limits` (optional): curried by the tool's author, never by a caller.
  Limits are part of the ArgTree, so changing them re-keys the tool, which is
  correct.

## The host: `worker-wasm`

One job runs like this:

1. `caos get /cas/args/module` (and `module-cache` if it is bound). Read the
   bytes.
2. Create an isolate with a heap limit. Compile the module, or deserialize the
   cache if it matches this V8 build, and fall back to compiling if it does
   not.
3. Instantiate with two import modules: `wasi_snapshot_preview1` (implemented
   by the host, over the filesystem view below) and `caos` (the verbs below).
   An import outside these two is refused at instantiation.
4. Call `_start`. The argv is the module name; the environment is empty.
5. Map the outcome to a job result:
   - `proc_exit(0)` with `/cas/out` recorded is success.
   - A nonzero exit or a trap fails the job with the stderr tail.
   - The watchdog's `TerminateExecution` (wall or CPU limit) also fails the
     job.
   - As with every failure, none of these is cached.
6. Drop the isolate and exit. The runner forks `/worker` per job, as it does
   today.

**It stays an unprivileged process that shells out to the setuid `/bin/caos`.**
The host must NOT link the `caos` library and write `/cas` itself. `/cas` is
root-owned so that no worker can tamper with content-addressed files
(`SPEC.md`, "Workers"), and the host is a worker.

- Each verb costs one fork and exec, on the order of milliseconds.
- If that cost dominates, the fix is one long-lived setuid `caos` per job
  serving verbs over a pipe, as `caos next --stream` already does for jobs.
  Measure before building it.

### Which V8

- **Production: a Rust binary on the `v8` crate (rusty_v8).**
  - It gives isolate heap limits, `terminate_execution` from a watchdog
    thread, and compiled-module serialization (the code cache).
  - No JS is involved at all: the imports are Rust callbacks.
  - It lives in the workspace next to the other `worker-*` crates.
  - Cost: rusty_v8 links a large prebuilt static `librusty_v8.a`, and as far as
    I know upstream publishes it for glibc only, not musl. So this host cannot
    ride the musl-static `std/runner`. It gets its own flake image
    (`std/wasm/flake.nix`, the way `std/go` has one), and nix supplies
    `RUSTY_V8_ARCHIVE` so cargo stays `--offline`.
  - The V8 closure is heavy. Build it as a `#depsImage` memo
    (`design/flake-deps-image.md`), so that a host edit re-keys a thin layer
    rather than the whole closure. `std/cargo` uses the same arrangement for
    `worker-cargo`.
- **Spike: Node 22 from nixpkgs plus a ~500-line JS host** that implements WASI
  preview1 over synchronous `node:fs`.
  - Not `node:wasi`: it is uvwasi underneath, so `path_open` cannot be
    intercepted for lazy fetch, and Node documents it as not being a sandbox.
  - It is good for settling the guest ABI and the compile pipeline in days.
    The ABI is the same either way, so no guest changes when the Rust host
    replaces it.
- **Not deno_core:** it brings an ops system and module loader we would not
  use.
- **The ABI is engine-neutral.** If V8 stops paying for itself, wasmtime could
  implement the same imports. See open question 1.

## The guest's view: files (WASI preview1)

| guest path | backed by | access |
|---|---|---|
| `/cas` | `/cas` | read-only, lazy |
| `/tmp` | `/tmp/wasm-guest` (wiped between jobs by the runner's reset) | read-write |
| `/secret` | `/secret`, only if present | read-only |

**Lazy `/cas`.**

- `path_open`, `fd_readdir` and `path_filestat_get` under `/cas` walk the path
  one component at a time.
- The first time a component is touched, the host runs `caos get <prefix>`
  (one level, never `-r`) and remembers that it did.
- The host can tell a placeholder without asking: it is mode `r--------`
  until a `get` makes it world-readable, and every `/cas` path carries its
  hash in the `user.caos.hash` xattr (README, "The CAS and /cas").
  `fd_filestat_get` can report the real file type from the tree entry, so
  `readdir` never forces a fetch.
- So `std::fs::read("/cas/args/in/src/main.rs")` just works, and fetches only
  the levels it walks. bash-tool's `paths` footgun (std/README, "Using
  bash-tool") does not exist here.
- Writes under `/cas` return `EROFS`. A result goes out through `caos.put`.

**Symlinks.**

- A guest may `path_symlink` inside `/tmp` to a target in `/cas`. `caos put`
  then stores that entry by its recorded hash without reading it, which is the
  staging trick `worker-rustc` plays with `link`.
- The host refuses symlink targets outside `/cas` and `/tmp`.

**Path translation.** Guest `/tmp/x` is host `/tmp/wasm-guest/x`. The host
rewrites paths before passing them to a verb, for example `put`'s source.

**The rest of preview1:**

- `fd_write` on 1 and 2 goes to the job log, which the runner already relays
  and masks.
- `clock_time_get` and `random_get` are real (see "Determinism").
- `poll_oneoff` supports clock sleeps only.
- `sock_*` returns `ENOSYS`.
- There are no processes and no threads in v1.

## The guest's view: the `caos` import module

**It mirrors the `/bin/caos` worker CLI verb for verb.** There is one source of
truth for semantics, and the host only argv-forwards. The raw ABI is tiny and
versioned by its module name:

```
(import "caos_v1" "call"       (func (param $argv i32) (param $len i32) (result i32)))  ;; NUL-separated argv → exit status
(import "caos_v1" "out_len"    (func (result i32)))                                      ;; stdout of the last call
(import "caos_v1" "out_read"   (func (param $ptr i32) (param $len i32) (result i32)))
(import "caos_v1" "err_len"    (func (result i32)))                                      ;; stderr of the last call
(import "caos_v1" "err_read"   (func (param $ptr i32) (param $len i32) (result i32)))
```

**Allowlisted verbs:**

- `get`, `kind`, `hash`, `put`, `put-commit` and `forward`
- `curry` and `prepare-request`
- `map-then`, `run-then`, `run-request-then` and `eval-path-then`
- `sub-run`

**Refused:** `runner`, `next`, `import-git` and `push-git`. Those are
privileged or reach the network, and each can be added on purpose later.

**`caos-guest` wraps the ABI** in the `worker-common` vocabulary: `arg`,
`read_arg`, `read_arg_opt`, `put`, `hash`, `curry`, `prepare_request`,
`map_then`, `run_then`, `run_then_catching`, `eval_then`, `own_image`,
`curry_self`, and `output_text` / `output_tree` / `report`. Its `Arg` enum
(`Lit`, `Path`, `Hash`) matches `worker-common`'s, so porting a worker is
mechanical.

**Later:** a WIT world (`caos:worker/host`) once the component model is worth
its toolchain. V8 has no native component model; it would go through `jco`
transpilation or a host-side adapter. Core modules plus preview1 are enough for
v1.

## Sub-work is a tail call

No worker may block on another (`design/map-then.md`, "Why this cannot
deadlock"), and a guest is a worker.

- `run_then` and its siblings record a continuation at `/cas/out`, and the
  guest must then return.
- The host enforces this: after a continuation verb succeeds, another
  `/cas/out` write is an error, and so is a nonzero exit.

There are two programming models.

**A. Staged (v1): the `stage` arg, as `worker-rustc` uses it.** The guest
curries itself as `then`, and on re-entry reads `/cas/args/result` (or
`/cas/args/error` under catch):

```rust
use caos_guest::{self as caos, Arg};

fn main() -> caos::Result<()> {
    match caos::read_arg_opt("stage")?.as_deref() {
        None => {
            let then = caos::curry_self(&[("stage", Arg::Lit("finish"))])?;
            caos::run_then("/cas/args/in", Arg::Path("/cas/args/tool"), Some(then))
        }
        Some("finish") => {
            let r = std::fs::read_to_string("/cas/args/result")?; // lazy fetch
            caos::output_text(&format!("tool said: {r}"))
        }
        Some(other) => Err(format!("unknown stage {other:?}").into()),
    }
}
```

`map_then` with `map = curry_self(stage=leaf)` is the `file-count` fold,
unchanged.

**B. Suspend and resume (later, optional): make `caos.run(...)` look
blocking.**

- At build time, binaryen's asyncify pass lets the host unwind the guest's
  stack into linear memory.
- On the call, the host snapshots memory, globals and the asyncify buffer
  into a tree of 64 KiB page blobs (zero pages left out, repeated pages
  deduplicated by git). It records `run-then` with
  `then = curry(own image, resume=<snapshot>)` and exits.
- The resumed job instantiates, restores memory, rewinds, and the import
  returns R.
- The snapshot is content-addressed, so a deterministic guest stays a cache
  hit.
- Costs: asyncify grows code and slows it, roughly 20–50%, and snapshots cost
  as much as the memory the guest touched.
- JSPI does not help here. It suspends inside one isolate, and the isolate
  dies with the job.
- Build B only if staged code turns out to be painful.

## Returning a result

The guest builds its output in `/tmp` and calls
`caos.put("/tmp/out", "/cas/out")`, or puts a blob. The three result
conventions (`SPEC.md`, "Returning a result") and the writer shape
`{prop, out, message?, failed?}` apply unchanged, because to the harness a wasm
tool is just a tool.

## Limits and isolation

- **Memory.** The isolate has a heap limit, and linear memory is capped at the
  smaller of the module's declared max and `wasm-limits`.
- **Time.** A watchdog thread calls `terminate_execution`. The failure is
  uncached.
- **Network.** The guest has none. The host container still has the network,
  because it must reach the server, but the guest reaches the server only
  through the allowlisted verbs.
- **Underneath.** The container and the unprivileged worker uid still apply,
  as defence in depth.

## Determinism

- In v1, clocks and random are real, the same as any other worker. The cache
  tolerates "deterministic enough" (`SPEC.md`, "Principles of reliability").
- Later, `wasm-limits` could add `deterministic=1`:
  - the clock is pinned and `random_get` is seeded from the ArgTree hash
  - a module is then a pure function by construction: replaying an ArgTree
    reproduces byte-identical output
  - that is useful for audit, and for trusting a cache entry from someone else

## Warmth and compile cost

- **Runner matching.** Every wasm tool has the same `base`, so the runner's
  `{base}` poll matches any wasm job. Per-module warmth (compiled code) is
  handled by the code cache below, not by matching.
- **Process per job.** Resident mode is per-affinity, one job at a time per
  key. Keying it by module would serialize a module's `map-then` fan-out,
  which is wrong. V8 starts from its built-in snapshot in milliseconds.
- **Compiling.**
  - Liftoff compiles quickly and lazily, and TurboFan tiers up in the
    background, so most modules need nothing more.
  - For big modules, there is `module-cache`: a `std/wasm-precompile` job
    keyed on (host image, module) returns V8's serialized compiled module, and
    the factory curries it beside `module`.
  - The host validates the cache and falls back to compiling. V8 rejects a
    cache built with a different version, flags or CPU features.
  - Because the job is keyed on the host image, a V8 bump re-keys every cache
    by itself. Measure before building it.
- **Later: an isolate pool.** One host process would run N isolates at once.
  That is where isolates pay off most, but it needs the runner to hand one
  process several concurrent jobs, which is a change to
  `design/runner-protocol.md`. It is out of scope here.

## Compiling to wasm

Rust comes first: the workspace is Rust and `worker-rustc` is the model.

- **`std/cargo`**
  - Add `wasm32-wasip1` to the pinned toolchain's targets (rust-overlay's
    `targets`).
  - Bake `caos-guest` and its dependencies for that target at the release
    profile. That needs a separate target directory: `bake.nix` already keeps
    musl and host apart because they cannot share one `target/`.
  - It stays `--offline`.
  - **Caveat:** cargo, rustc and runner are part of the SEEDED core
    (`stack/build-builtins.sh`), and `rust/rust-toolchain.toml` pins musl
    targets only. Adding a target there re-keys the core bake for every
    tool.
  - The alternative is a separate `std/cargo-wasm` image. It would use the
    same toolchain plus the wasm target, and bake only `caos-guest`'s
    dependency graph.
  - Recommendation: start with `std/cargo-wasm`, and fold it into
    `std/cargo` only if keeping two bakes in sync hurts.
- **No C toolchain exists today.** `std/go` has `CGO_ENABLED=0`, and the
  bake's musl cc is internal, so a wasi-sdk image is new work, not a reuse.
- **`rust/crates/caos-guest`**: the guest crate above. It must not depend on
  `std::process`.
- **`worker-rust-wasm`**: the factory. It runs on `std/runner`, like rustc.
  - `start` lays out the project as CAS links: a single `src` file or a
    project directory, with `caos-guest` spliced in at `caos-guest/` and
    `DEEP-DEPS`, `.caos-expr` and `DEPS` skipped, all exactly as
    `worker-rustc` does it.
  - Then it tail-calls cargo with
    `build --target wasm32-wasip1 --release`.
  - `finish` takes `target/wasm32-wasip1/release/*.wasm` and optionally runs
    `wasm-opt -O`.
  - It then **validates the imports**: only `wasi_snapshot_preview1.*` and
    `caos_v1.*` are allowed. A tool importing anything else fails in the
    factory with a readable error, not at run time.
  - It emits `curry(wasm, module=<blob>)`.
- **Other languages**, each a factory that emits the same curry:
  - **Go:** `GOOS=wasip1 GOARCH=wasm` (Go 1.21 and later), with
    `//go:wasmimport caos_v1 call`. `std/go` already has the toolchain.
    TinyGo is the option for small modules.
  - **C/C++:** a wasi-sdk (clang plus wasi-libc) flake image.
  - **Zig, AssemblyScript:** when someone asks.

## Example: hello-wasm

```rust
// std/hello-wasm/src/main.rs
use caos_guest as caos;

fn main() -> caos::Result<()> {
    let name = caos::read_arg_opt("name")?.unwrap_or_else(|| "world".into());
    let readme = std::fs::read("/cas/args/in/README.md")?;      // lazy: fetches 3 levels
    std::fs::write("/tmp/out.txt", format!("hello {name}: README is {} bytes\n", readme.len()))?;
    caos::put("/tmp/out.txt", "/cas/out")                        // a blob: printed verbatim
}
```

## Plan, as a stack

1. `01-wasm-host-spike`
   - `std/wasm` on Node, with the JS WASI host.
   - `tests/wasm-hello` runs a checked-in `.wasm` built from `.wat`: args,
     lazy `/cas`, `/tmp`, `put`.
2. `02-rust-wasm`
   - `caos-guest`, the `wasm32-wasip1` target in `std/cargo`, the
     `std/rust-wasm` factory and `std/hello-wasm`.
   - `tests/wasm-rust` covers a `run-then` round trip, a `map-then` fan-out,
     a failure that surfaces stderr, a refused verb and a refused import.
3. `03-v8-host`: `worker-wasm` on rusty_v8 replaces the spike, adding heap
   limits, the watchdog and a `wasm-limits` test.
4. `04-module-cache`, once a measurement says compile time matters.
5. `05-go-wasm` and `wasi-sdk` factories.

Later: suspend and resume (model B), the isolate pool, the deterministic mode
and WIT.

## Open questions

1. **Is V8 a requirement, or is the requirement "a fast in-process sandbox"?**
   - Wasmtime has WASI p1/p2 natively, fuel and epoch interruption, and AOT
     `.cwasm`, which would be a cleaner `module-cache`.
   - It also builds musl-static, so the host could ride `std/runner` with no
     image of its own.
   - V8's edges are JS guests on the same host (see 3) and its isolate model if
     we go multi-tenant.
2. **Network for guests?** The default is none. If we want it, the place is a
   `net` capability in `wasm-limits` that grants a host-mediated fetch.
3. **Accept JS as well as wasm?** With V8 the same host could take a `.js`
   `module` in the same isolate, for very little.
4. **The fork-per-verb cost** of `/bin/caos`. Measure it in the spike.
5. **Should the deterministic mode be the default** for new wasm tools?
