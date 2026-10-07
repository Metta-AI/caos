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
| `bash-tool` | Run `sh -c` over a scratch copy of the conversation tree, to compute something over its content. List in `paths` every existing file or directory the command touches, or it sees an empty placeholder ([Using `bash-tool`](#using-bash-tool)). |
| `caos-build` | Compile the caos tree with `nix build`, against a persisted nix store, returning the build log. |
| `caos-conversation` | Read a recorded conversation (e.g. a Claude Code session) from the hash of its tip commit: messages, and each tool call with its arguments and result. For reviewing how a session went. |
| `caos-conversation-list` | List the recorded conversations on the server, newest first: time of the last message, id, tip hash, title and who has each. The tip hash is what `caos-conversation` takes. |
| `caos-test` | Build the caos test stack from the tree and run the whole suite — unit tests and every `tests/<name>`. |
| `caos-test-result` | Print one test's COMPLETE record from a `caos-test` run, where the report carries only an excerpt. |
| `read-trace` | Print the trace of a run (`/status/<hash>?all=1`) from the ArgTree hash `caos-test` prints in its "full trace" line: timings per node, the slowest nodes, and with `perf` the workers' out-trace data. |
| `merge` | Three-way merge another commit into the current source tree. |
| `create-squashed-stack` | Squash a stack to one commit per layer, as a plan file says, and write them to a folder of gitlinks for publishing. |
| `github` | Call the GitHub API with the granted token: find, open and update PRs, link a stack. One run is one API call. |

A tool's INPUT is the source tree its path lies in, so a tool reached at
`caos-std/<name>` operates on the conversation's own files. `tool_help` says
what each one does about that.

### Before you reach for the shell

Most of what a shell is used for here is already a tool, and none of these
needs `paths`:

| to | use |
|---|---|
| read, list or search | `read`, `ls`, `grep` |
| create or change a file | `write`, `edit` |
| copy a file or directory (a source tree stays a source tree) | `copy` (`from`, `to`) |
| move or rename | `move` (`from`, `to`) |
| delete | `remove` (`file-path`) |
| see history or changes | `log`, `show`, `diff` |

`copy` and `move` create missing parent directories and refuse an existing
destination. They are registered by `llm-step`, so they appear in the model's
tool list; they are not entries under `std/`.

### Using `bash-tool`

`bash-tool` is for computing something over content: counting, sorting,
`diff -r`, running a script. It runs your command in a scratch directory built
from the conversation tree. The tree is lazy: a file or directory shows up with
its contents only if you name it in `paths`. Everything else is a placeholder
link into `/cas`, which is why a model that has never seen this fails in
different-looking ways (all of this is measured, with nothing in `paths`):

| you ran | what you see |
|---|---|
| `cat`, `cp`, `rm`, `ls`, `grep -r`, `find` on the placeholder's path | `Permission denied`, plus a line saying which `paths` to retry with |
| `cd` into it | `Not a directory`, and the rest of the command runs from the root |
| `grep -r .`, `find .`, `ls -R` from the root, or after that failed `cd` | **nothing**, and exit 1 from grep: they do not follow the links, so "no matches" is not an answer |
| `ls -l` on its parent | a link to `/cas/args/in/...` |

All of them mean the same thing: add the path to `paths` and run it again. A
directory in `paths` brings everything under it, source trees included, so
`"paths": ["imports/repo/base"]` is enough for a recursive search of it.

```
run_tool(path="caos-std/bash-tool",
         arguments={"cmd": "find imports/repo/base -name '*.md' | xargs wc -l",
                    "paths": ["imports/repo/base"]})
```

- **New files and new directories need no `paths`.** `mkdir -p` the parent of
  anything you create.
- **There is no commit step.** What the command writes is saved, even if the
  command then exits non-zero, and editing files in a source tree creates its
  child commits.
- **Not in the shell:** `git` is not installed (use `log`, `show` and `diff`
  on the source tree), and `caos-std/` is not in the conversation tree at all,
  because it exists only in the evaluated tree (`eval_path`, then `read` or `ls`
  with `root`). The harness's own shell is switched off.

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
| `actor` | The actor wrapper: runs an inner `(state, message) -> (state', reply)` request against state on a Git branch, publishing by moving the branch with a compare-and-swap ([`actor/README.md`](actor/README.md)); a Go program on `std/go`. |
| `hello` | The smallest possible entry, used by `tests/hello` and by hand when something is deeply broken. |
| `llm-stub` | A scripted stand-in for the model, so `tests/llm-*` run with no API key and no network. |
| `llm-test` / `llm-test-tool` | Fixtures the llm tests drive: a test harness entry and a tool for it to call. |
