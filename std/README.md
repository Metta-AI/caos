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
| `read` | Read a file's contents, through code references (`feature/dirty/README.md`). Truncated at 100KB; page with `offset` and `limit`. |
| `ls` | List a directory; source trees and directories end in `/`. |
| `write` | Write a file, creating parent directories. Inside a source tree it makes a child commit. |
| `edit` | Replace exact text in a file, once or everywhere. |
| `copy` | Copy a file or directory; a source tree stays a source tree, with its history. |
| `move` | Move or rename; a source tree stays a source tree. |
| `remove` | Remove a file or directory; removing a source tree removes it whole. |
| `grep` | Search a tree with a regular expression, through code references; matches as `path:linenum:line`. Results are cached per unchanged subtree. |
| `log`, `show`, `diff` | A source tree's git history: commits, one commit with its diff, or a diff between two revisions. Take `source-tree`, the path of the code reference. |
| `caos-build` | Compile the caos tree with `nix build`, against a persisted nix store, returning the build log. A router to the stack daemon's `build` op. |
| `caos-conversation` | Read a recorded conversation (e.g. a Claude Code session) from the hash of its tip commit: messages, and each tool call with its arguments and result. For reviewing how a session went. |
| `caos-conversation-list` | List the recorded conversations on the server, newest first: time of the last message, id, tip hash, title and who has each. The tip hash is what `caos-conversation` takes. |
| `caos-test` | Run the whole suite — unit tests and every `tests/<name>` — on the tree's stack, building and starting it if it is not up. |
| `caos-test-result` | Print one test's COMPLETE record from a `caos-test` run, where the report carries only an excerpt. |
| `read-trace` | Print the trace of a run (`/status/<hash>?all=1`) from the ArgTree hash `caos-test` prints in its "full trace" line: timings per node, the slowest nodes, and with `perf` the workers' out-trace data. |
| `merge` | Three-way merge another commit (a hash, or the path of an imported source tree) into the source tree you name with `source-tree`. |
| `import-source` | Import an HTTPS Git repository as a new snapshot at an unused conversation path (a source tree). The remote is observed when the call starts, so a fresh `into` is how you see a branch's current head. |
| `publish-source` | Publish a source tree's exact commit to an HTTPS Git repository branch, preserving history; leased on the remote head observed at the start. Does not open a PR (`github` does). |
| `agent-spawn` | Start a focused coding agent in a durable child conversation, with an isolated copy of the conversation (or the `paths` you select). Returns the child id. |
| `agent-wait` | Wait for one child agent to report a terminal checkpoint, by joining its durable relay. |
| `agent-harvest` | Apply a terminal child's content changes to the conversation, with the same atomic reconciliation as the file tools. |
| `run-async` | Start or inspect an already-built request (an ArgTree hash) in the background; repeating it reads the same durable task. |
| `stack-publish` | Publish a stack directory in one call: push every described layer (`NN-<name>.pr.md` beside the gitlink) as a branch named without its number, find or open the chained PRs, and link them as a GitHub stack. No squashing. |
| `stack-sync` | Bring a stack directory up to date with its remote, as `gt sync` does, without pushing: the trunk's head becomes `00-base`, a branch GitHub rewrote is adopted under the layer's edits, foreign pushes are merged in, every layer is restacked onto the one below, and merged layers are reported. Then `stack-publish` fast-forwards. |
| `move-sed` | Rename the entries of one conversation directory by regular expression, in one proposal: a layer and its `.pr.md` move together. |
| `create-squashed-stack` | Squash a stack to one commit per layer, as a plan file says, and write them to a folder of gitlinks for publishing. Not part of the publishing flow (`stack-publish` pushes layers as they are). |
| `github` | Call the GitHub API with the granted token: find, open and update PRs, link a stack. One run is one API call. |
| `flake-update` | Re-lock a flake by running `nix flake update` in a worker: every input, one input, or one input pinned to a commit. Takes a tree holding flake.nix and flake.lock; returns the new flake.lock. |

A tool's INPUT is the source tree its path lies in, so a tool reached at
`caos-std/<name>` operates on the conversation's own files. `tool_help` says
what each one does about that.

### Git advice: [GIT.md](GIT.md)

For anything involving git or GitHub — importing a repo, merging, stacks,
publishing a stack with `publish-stack`, `publish_source`, PRs and `github` — read
[`GIT.md`](GIT.md) first.

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

`read`, `ls`, `write`, `edit`, `copy`, `move`, `remove`, `grep`, `log`, `show` and `diff`
are entries under `std/`, reached as `caos-std/<name>` like any other tool. They take an
`in` tree, which defaults to the conversation. `llm-step` still registers its own copies
for the turns it drives itself; those are being retired in favour of these.

`import-source`, `publish-source`, `agent-spawn`, `agent-wait`, `agent-harvest` and `run-async`
are entries for OPERATIONS OF THE STEP (`@op` in their help): they write the conversation's own
records (a source tree attached, a publication pinned before anything is pushed, a child
conversation, an async task), so the step performs them in process. The entry gives the
operation a path, a help and a place in `tool_help`/`run_tool`.
`eval-path` is not an entry: evaluating a path is one of the server's primitives (the
`/eval` route), so it is a built-in tool, like `run`, `help`, `start` and `result`.

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
`"paths": ["sources/repo/00-base"]` is enough for a recursive search of it.

```
run_tool(path="caos-std/bash-tool",
         arguments={"cmd": "find sources/repo/00-base -name '*.md' | xargs wc -l",
                    "paths": ["sources/repo/00-base"]})
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

Workers have the network. Every worker container runs on the compute network:
it reaches the caos server over HTTP, `llm-client` posts to
`api.anthropic.com`, `flake-builder` fetches flake inputs and pushes to the
registry, and `go` ships a CA bundle so a script's own HTTPS request verifies.
Where a worker is offline, that is the build's choice (`cargo` below), never a
property of the container. A tool that has to fetch something can.

| entry | one line |
|---|---|
| `bash` | The script worker: shell plus coreutils, `/worker` runs the `worker1` script curried onto it. Not the default for a new worker: that is `go`, below. Bash remains for the interpreters' own `/worker` scripts and for glue that only sequences other programs. |
| `go` | The Go script worker: `/worker` `go run`s `worker1` against a primed build cache. A script imports `caos/w`: an error policy (`Main`, `Check`, `Must`, `True`) and the worker CLI (`Caos`, `Get`, `Read`, `Put`, `Hash`, `Exists`), so a script does not define its own. |
| `cargo` | The cargo worker: pinned toolchain, baked dependencies, vendored registry. It builds `--offline` by choice, so a missing dep fails instead of being fetched; the container has the network. |
| `rustc` | The worker factory that turns a Rust project into a runnable worker; part of the seeded core. |
| `runner` | The pooled interpreter every compiled worker is curried onto; part of the seeded core. |
| `deep-deps` | Mounts each directory's `DEPS` at `DEEP-DEPS/<name>`, which is what lets a subtree name things outside itself. |
| `flake-builder` | Builds a flake directory into a runnable image. |
| `flake-input-loader` | Splices a flake input's tree into the consumer's own, so a repo reaches caos' `std/` by path. |
| `git-runner` | An opt-in git runtime for compiled workers, for the one thing caos has no verb for. |
| `llm-step` | The agent harness: one turn of a conversation, and the registry of tools it offers a model. |
| `llm-client` | The Anthropic API client `llm-step` posts through. |
| `llm-call` | A single model call as an entry, for an expression that wants one without a conversation. |
| `tree-map` | Runs a worker (its `run` arg) over every directory of a tree and returns a sparse tree of the results, same shape as the input, one cached job per directory. Other args are forwarded to the worker. `rgrep` is built on it. |
| `rgrep` | The search worker behind the step's `grep` tool: `tree-map` over a Go grep of one directory (`rgrep/level`). |
| `run-and-update-ref` | The async worker: one binary, two stages, behind `run_async` and the subagent tools. |
| `actor` | The actor wrapper: runs an inner `(state, message) -> (state', reply)` request against state on a Git branch, publishing by moving the branch with a compare-and-swap ([`actor/README.md`](actor/README.md)); a Go program on `std/go`. |
| `hello` | The smallest possible entry, used by `tests/hello` and by hand when something is deeply broken. |
| `llm-stub` | A scripted stand-in for the model, so `tests/llm-*` run with no API key and never call the API. |
| `llm-test` / `llm-test-tool` | Fixtures the llm tests drive: a test harness entry and a tool for it to call. |
