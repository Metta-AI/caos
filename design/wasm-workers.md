# Wasm workers — WebAssembly in a V8 isolate, as a caos worker

**Status:** sketch (2026-W45-5). Nothing is built. This is the shape to argue
about before a spike. Running many jobs in one host process is a separate
design, `design/multiplexed-workers.md`, that this one builds on.

## Goal

- A worker can be a `.wasm` module. A Node host runs it in a V8 isolate, and
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
  and can start processes (std/README, "Workers have the network"). A wasm
  guest can call only its imports: two preopened directories and an allowlist
  of caos verbs.
- **One warm image for every wasm tool.** Every wasm tool shares the same
  `base`, so any warm host takes any wasm job, as `std/runner` already does for
  compiled binaries.
- **Many jobs per process.** Isolates are cheap, so with multiplexed workers
  one host process runs many jobs at once, and compiles each module once.
- **Small artifacts that run on any CPU.** One `.wasm` runs on x86_64 and
  aarch64 runners, with no per-target build.
- **Many languages, one runtime:** Go, Rust, C/C++, Zig and AssemblyScript.

## Shape: an interpreter image, like std/bash and std/go

| today | wasm |
|---|---|
| `std/bash`: `/worker` runs the `worker1` script | `std/wasm`: `/worker` (Node) instantiates the `worker1` module in an isolate |
| `std/rustc`: `src` → `curry(runner, worker1=<bin>)` | `std/go-wasm`, `std/rust-wasm`: `src` → `curry(wasm, worker1=<x.wasm>)` |
| `std/go/prelude/w` (the Go worker helpers) | a guest SDK per language, shipped with its factory |

**The module is `worker1`.** In the `workerN` convention, `worker1` is the
worker, logically, whether the interpreter below it execs it (`std/runner`),
hands it to bash (`std/bash`), or instantiates it (`std/wasm`). Nothing new is
reserved.

A tool is then as small as `std/hello`:

```
# std/hello-wasm/DEPS
../go-wasm go-wasm

# std/hello-wasm/.caos-expr
HELP=<<END
Greet, and count the bytes of a README.
@param [name] Who to greet.
@in
END
IMAGE=run --base:@=DEEP-DEPS/go-wasm --src:@=.
curry --base=$IMAGE --help=$HELP
```

A prebuilt module needs no factory:
`curry --base:@=DEEP-DEPS/wasm --worker1:@=tool.wasm`.

One optional arg is new: `wasm-limits`, curried by the tool's author and never
by a caller. Limits are part of the ArgTree, so changing them re-keys the tool,
which is correct.

## The host: Node

`std/wasm` is a flake image (like `std/go`): Node from nixpkgs plus the host
script as `/worker`. No Rust: the codebase is moving away from it, and Node is
real V8 with real isolates.

**One job is one `worker_thread`, and each `worker_thread` is its own V8
isolate.** `node:vm` contexts are not enough: they share one isolate.

One job runs like this:

1. `caos get /cas/args/worker1`. Read the bytes.
2. Compile it to a `WebAssembly.Module` on the main thread, or reuse the one
   already compiled for that `worker1` hash in this process.
3. Start a `worker_thread` with `resourceLimits` (heap) from `wasm-limits`,
   and post it the compiled module. Node can send a compiled
   `WebAssembly.Module` to another thread, so a module compiles once per
   process, however many jobs use it. Confirm this in the spike.
4. In the thread, instantiate with two import modules: `wasi_snapshot_preview1`
   (implemented by the host in JS, over the filesystem view below) and
   `caos_v1` (the verbs below). An import outside these two is refused at
   instantiation.
5. Call `_start`. The argv is the module name; the environment is empty.
6. Map the outcome to a job result:
   - `proc_exit(0)` with `/cas/out` recorded is success.
   - A nonzero exit or a trap fails the job with the stderr tail.
   - The watchdog (wall or CPU limit) calls `worker.terminate()`, which also
     fails the job.
   - As with every failure, none of these is cached.

Until multiplexed workers exist, the host runs one job and exits, and the
runner forks `/worker` per job as it does for every image. With them, one host
process keeps a pool of threads, one per job in flight
(`design/multiplexed-workers.md`).

**WASI is the host's own JS, not `node:wasi`.** `node:wasi` is uvwasi
underneath, so `path_open` cannot be intercepted for lazy fetch, and Node
documents it as not a sandbox. Preview1 over synchronous `node:fs` is a few
hundred lines.

**The host stays an unprivileged process that runs the setuid `/bin/caos`**
(`execFileSync`, which works from a worker thread). It must not write `/cas`
itself: `/cas` is root-owned so that no worker can tamper with
content-addressed files (`SPEC.md`, "Workers"), and the host is a worker.

- Each verb costs one fork and exec, on the order of milliseconds.
- If that cost dominates, the fix is one long-lived setuid `caos` serving
  verbs over a pipe. Measure in the spike before building it.

**A `worker_thread` is a full Node environment** (fs, child_process). That is
fine for a wasm guest, which reaches only its imports. It is not a sandbox for
JS guests: those would need a `vm` context with no Node globals inside the
thread (open question 2).

## The guest's view: files (WASI preview1)

| guest path | backed by | access |
|---|---|---|
| `/cas` | the job's CAS directory | read-only, lazy |
| `/tmp` | a per-job directory under `/tmp` (wiped by the runner) | read-write |
| `/secret` | the job's secrets, only if present | read-only |

"The job's" is `/cas` and `/secret` today. Under multiplexed workers it is
`/cas/jobs/<id>` and `/secret/<id>`, and the guest cannot tell the difference.

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
- So `os.ReadFile("/cas/args/in/src/main.go")` just works, and fetches only
  the levels it walks. bash-tool's `paths` footgun (std/README, "Using
  bash-tool") does not exist here.
- Writes under `/cas` return `EROFS`. A result goes out through `caos.put`.

**Symlinks.**

- A guest may `path_symlink` inside `/tmp` to a target in `/cas`. `caos put`
  then stores that entry by its recorded hash without reading it, which is the
  staging trick `worker-rustc` plays with `link`.
- The host refuses symlink targets outside `/cas` and `/tmp`.

**Path translation.** Guest paths map to the job's host directories, and the
host rewrites them before passing them to a verb, for example `put`'s source.

**The rest of preview1:**

- `fd_write` on 1 and 2 goes to the job log.
- `clock_time_get` and `random_get` are real (see "Determinism").
- `poll_oneoff` supports clock sleeps only.
- `sock_*` returns `ENOSYS`.
- There are no processes and no threads in v1.

## The guest's view: the `caos_v1` import module

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

**Refused:** `runner`, `next`, `jobs`, `import-git` and `push-git`. Those are
privileged or reach the network, and each can be added on purpose later.

**Each guest SDK wraps the ABI** in the vocabulary the Go prelude
(`std/go/prelude/w`) and `worker-common` already use: `Arg`, `ReadArg`,
`Put`, `Hash`, `Curry`, `PrepareRequest`, `MapThen`, `RunThen`, `EvalThen`,
`OwnImage`, `CurrySelf`, and `OutputText` / `OutputTree` / `Report`. Porting a
worker is then mechanical.

**Later:** a WIT world (`caos:worker/host`) once the component model is worth
its toolchain. V8 has no native component model; it would go through `jco`
transpilation. Core modules plus preview1 are enough for v1.

## Sub-work is a tail call

No worker may block on another (`design/map-then.md`, "Why this cannot
deadlock"), and a guest is a worker.

- `RunThen` and its siblings record a continuation at `/cas/out`, and the
  guest must then return.
- The host enforces this: after a continuation verb succeeds, another
  `/cas/out` write is an error, and so is a nonzero exit.

There are two programming models.

**A. Staged (v1): the `stage` arg, as `worker-rustc` uses it.** The guest
curries itself as `then`, and on re-entry reads `/cas/args/result` (or
`/cas/args/error` under catch):

```go
func main() {
    w.Main(func() {
        switch w.ReadArgOpt("stage") {
        case "":
            then := w.CurrySelf(w.Lit("stage", "finish"))
            w.RunThen("/cas/args/in", w.Path("/cas/args/tool"), then)
        case "finish":
            r := w.Must(os.ReadFile("/cas/args/result")) // lazy fetch
            w.OutputText("tool said: " + string(r))
        }
    })
}
```

`MapThen` with `map = CurrySelf(stage=leaf)` is the `file-count` fold,
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
  ends with the job.
- Build B only if staged code turns out to be painful.

## Returning a result

The guest builds its output in `/tmp` and calls
`caos.put("/tmp/out", "/cas/out")`, or puts a blob. The three result
conventions (`SPEC.md`, "Returning a result") and the writer shape
`{prop, out, message?, failed?}` apply unchanged, because to the harness a wasm
tool is just a tool.

## Limits and isolation

- **Memory.** Each job's thread has its own heap limit (`resourceLimits`), and
  linear memory is capped at the smaller of the module's declared max and
  `wasm-limits`.
- **Time.** A watchdog on the main thread calls `worker.terminate()`. The
  failure is uncached.
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
  `{base}` poll matches any wasm job.
- **Compiling.** Liftoff compiles quickly and lazily, and TurboFan tiers up in
  the background, so most modules need nothing more. Within a process, a
  module compiles once and is shared with every job thread (step 3 above).
- **Across processes** nothing is shared: Node has no public API to serialize
  a compiled wasm module. Multiplexed workers make this matter less, because
  one process lives across many jobs.
- **Resident mode is the wrong tool.** It is per-affinity, one job at a time
  per key, so keying it by module would serialize a module's `map-then`
  fan-out. Multiplexed workers are the mechanism instead.

## Compiling to wasm

The factories are Go workers on `std/go`, like `std/tree-map`. Each one lays
out the project as CAS links, tail-calls a toolchain, checks the result and
emits `curry(wasm, worker1=<blob>)`.

**Every factory validates the imports** before it emits: only
`wasi_snapshot_preview1.*` and `caos_v1.*` are allowed. A tool importing
anything else fails in the factory with a readable error, not at run time.

**Go first: `std/go-wasm`.**

- `GOOS=wasip1 GOARCH=wasm` (Go 1.21 and later), with
  `//go:wasmimport caos_v1 call` in the guest SDK.
- `std/go` already has the toolchain, so this needs no new image and does not
  touch the seeded core.
- TinyGo is the option if module size matters.

**Rust second: `std/rust-wasm`.**

- It needs `wasm32-wasip1`, and `rust/rust-toolchain.toml` pins musl targets
  only. cargo, rustc and runner are part of the SEEDED core
  (`stack/build-builtins.sh`), so adding a target to `std/cargo` re-keys the
  core bake for every tool.
- So it gets its own `std/cargo-wasm` image: the same toolchain plus the wasm
  target, baking only the guest SDK's dependencies, in its own `target/`.
- With Rust leaving the codebase, this is for Rust GUESTS only, and can wait
  until someone wants one.

**Others, when someone asks:**

- C/C++: a wasi-sdk (clang plus wasi-libc) flake image. No C toolchain exists
  today (`std/go` has `CGO_ENABLED=0`), so this is new work.
- Zig, AssemblyScript.

## Example: hello-wasm

```go
// std/hello-wasm/main.go
package main

import (
    "fmt"
    "os"

    w "caos/guest"
)

func main() {
    w.Main(func() {
        name := w.ReadArgOpt("name")
        if name == "" {
            name = "world"
        }
        readme := w.Must(os.ReadFile("/cas/args/in/README.md")) // lazy: fetches 3 levels
        w.Check(os.WriteFile("/tmp/out.txt",
            []byte(fmt.Sprintf("hello %s: README is %d bytes\n", name, len(readme))), 0o644))
        w.Put("/tmp/out.txt", "/cas/out") // a blob: printed verbatim
    })
}
```

## Plan, as a stack

1. `01-wasm-host`
   - `std/wasm`: Node, the JS WASI host and the `caos_v1` imports, one job per
     process.
   - `tests/wasm-hello` runs a checked-in `.wasm` built from `.wat`: args,
     lazy `/cas`, `/tmp`, `put`, a refused verb and a refused import.
2. `02-go-wasm`
   - The Go guest SDK, the `std/go-wasm` factory and `std/hello-wasm`.
   - `tests/wasm-go` covers a `run-then` round trip, a `map-then` fan-out, a
     failure that surfaces stderr, and a `wasm-limits` kill.
3. Multiplexed workers, in their own stack (`design/multiplexed-workers.md`).
4. `04-wasm-multiplexed`: the host runs N jobs on a thread pool under the
   multiplexed protocol, with in-process module sharing.
5. Rust and wasi-sdk factories, when wanted.

Later: suspend and resume (model B), the deterministic mode and WIT.

## Open questions

1. **Network for guests?** The default is none. If we want it, the place is a
   `net` capability in `wasm-limits` that grants a host-mediated fetch.
2. **Accept JS as well as wasm?** The same host could take a `.js` `worker1`
   in a `vm` context with no Node globals, inside the job's own thread. Cheap,
   but the sandbox would then rest on the host keeping Node out of that
   context.
3. **The fork-per-verb cost** of `/bin/caos`. Measure it in the spike.
4. **Should the deterministic mode be the default** for new wasm tools?
