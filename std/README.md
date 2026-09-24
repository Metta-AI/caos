# `std/` — what an agent can run, and what builds it

Two kinds of thing live here, and only the first is for a model to call.

## Tools

Each carries its own `HELP` and is reached BY PATH from a conversation whose
tree mounts caos — `caos-std/<name>` in a repo that loads caos through
`std/flake-input-loader` (`design/flake-inputs.md`, "Consumer root"). Nothing
registers them with the model: `run_tool` runs one and `tool_help` at the same
path prints its parameters, which is the description this table abbreviates.

| tool | one line |
|---|---|
| `bash-tool` | Run `sh -c` from the conversation root; ordinary files and source trees are writable, the rest is not. |
| `caos-build` | Compile the caos tree with `nix build`, against a persisted nix store, returning the build log. |
| `caos-test` | Build the caos test stack from the tree and run the whole suite — unit tests and every `tests/<name>`. |
| `caos-test-result` | Print one test's COMPLETE record from a `caos-test` run, where the report carries only an excerpt. |
| `merge` | Three-way merge another commit into the current source tree. |

A tool's INPUT is the source tree its path lies in, so a tool reached at
`caos-std/<name>` operates on the conversation's own files. `tool_help` says
what each one does about that.

## Images and workers

Not callable: these are what the tools and the build graph are made OF. An
expression names them (`--base:@=DEEP-DEPS/<name>`), and `caos-build` or the
test suite exercises them.

| entry | one line |
|---|---|
| `bash` | The script worker: shell plus coreutils, `/worker` runs the `worker1` script curried onto it. |
| `go` | The Go script worker: `/worker` `go run`s `worker1` against a primed build cache. |
| `cargo` | The cargo worker: pinned toolchain, baked dependencies, vendored registry, no network. |
| `rustc` | The worker factory that turns a Rust project into a runnable worker; part of the seeded core. |
| `runner` | The pooled interpreter every compiled worker is curried onto; part of the seeded core. |
| `deep-deps` | Mounts each directory's `DEPS` at `DEEP-DEPS/<name>`, which is what lets a subtree name things outside itself. |
| `flake-builder` | Builds a flake directory into a runnable image. |
| `flake-input-loader` | Splices a flake input's tree into the consumer's own, so a repo reaches caos' `std/` by path. |
| `git-runner` | An opt-in git runtime for compiled workers, for the one thing caos has no verb for. |
| `llm-step` | The agent harness: one turn of a conversation, and the registry of tools it offers a model. |
| `llm-client` | The Anthropic API client `llm-step` posts through. |
| `llm-call` | A single model call as an entry, for an expression that wants one without a conversation. |
| `rgrep` | The search worker behind the step's `grep` tool. |
| `run-and-update-ref` | The async worker: one binary, two stages, behind `run_async` and the subagent tools. |
| `hello` | The smallest possible entry, used by `tests/hello` and by hand when something is deeply broken. |
| `llm-stub` | A scripted stand-in for the model, so `tests/llm-*` run with no API key and no network. |
| `llm-test` / `llm-test-tool` | Fixtures the llm tests drive: a test harness entry and a tool for it to call. |
