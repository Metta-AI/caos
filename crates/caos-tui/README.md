# caos-tui

`caos-tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface keeps independent virtual conversations in a left sidebar. Each
has its own durable history, multiline prompt, live activity, completed-turn
hashes, and workspace diff. Turns continue running when another conversation
is selected, so several agent workspaces can advance concurrently without
touching the working checkout.

## Build and run

Run the client from a Git working tree whose `caos` remote points at a running
CAOS server:

```bash
git remote add caos http://localhost:9090
nix build .#caos-tui
./result/bin/caos-tui
```

During development, it can also be launched with `cargo run -p caos-tui`.

```text
caos-tui                  continue the most recent conversation
caos-tui --new            start a fresh conversation
caos-tui --from 5ec3751   branch from a completed turn
```

### Custom tools

A tool-set directory has one direct child per model-facing tool. The child
name becomes the tool name, `docs` is the description given to the LLM, and
`image` contains the runnable worker image reference:

```text
agent-tools/
└── lint/
    ├── docs
    └── image
```

Given a compatible worker image in `TOOL_IMAGE`, create and select a tool set
like this:

```sh
mkdir -p agent-tools/lint
printf '%s\n' \
  'Check the workspace. Pass an optional string `target` to limit the check.' \
  > agent-tools/lint/docs
printf '%s\n' "$TOOL_IMAGE" > agent-tools/lint/image
git add agent-tools

nix run .#caos-tui -- --new --tools agent-tools
```

The directory must be Git-tracked; like the rest of caos input ingestion,
untracked files are ignored. The inline file tools, `bash`, `grep`, and the
workspace's `caos-tools/*.sh` remain available. Press `Ctrl+T` in the TUI to
inspect project and configured tools separately.

A compatible worker receives a tree-valued `in` argument, materialized at
`/cas/args/in`, containing `workspace/` and an opaque `call.json`. Its docs must
tell the LLM which JSON fields to send. The worker writes `/cas/out` as a tree
containing a textual `result`, an optional `is_error` marker, and an optional
updated `workspace/`. An updated workspace must not contain a top-level `.caos`
entry. The `image` file may name a `/cas/std/...` worker, a content-addressed
Git image hash, or a `docker://...` image reference.

## Controls

| Input | Action |
|---|---|
| `Enter` | Send the prompt |
| `Alt+Enter` or `Ctrl+J` | Insert a newline |
| `Ctrl+Up` / `Ctrl+Down` | Select the previous or next conversation |
| `Ctrl+N` | Start a new virtual conversation |
| `Ctrl+Q` | Switch between conversation and workspace changes |
| `Ctrl+T` | Show the tools available to the selected conversation |
| `Ctrl+A` | Expand or collapse live Activity above the prompt |
| `PageUp` / `PageDown` | Scroll by rendered rows |
| Mouse wheel | Scroll by rendered rows |
| `Ctrl+Y` | Enter or leave terminal text-selection mode |
| `Ctrl+L` twice | Load the selected conversation into the working tree |
| `Ctrl+P` twice | Push the selected conversation as a clean branch and open a PR |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Exit |

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Activity entries show the durable hashes of internal harness steps for
inspection; those step trees contain harness metadata and are not branch
points.

`Ctrl+Y` releases mouse capture and freezes redraws so terminal-native text
selection remains stable. Drag across any visible text, use the terminal's
normal copy shortcut (`Cmd+C` on macOS or usually `Ctrl+Shift+C` elsewhere),
then press `Ctrl+Y` or `Escape` to resume the live interface.

## Workspace safety

Agent workspaces remain virtual commit trees under independent conversation
refs. Opening, switching, and running conversations never overwrite the working
checkout. Loading changes requires two `Ctrl+L` presses, a clean working tree,
and a successful `git apply --check` before the patch is applied.

Publishing also leaves the checkout untouched. Two `Ctrl+P` presses create or
advance `caos/<conversation>` with clean snapshot commits, push that branch to
`origin`, and use the authenticated `gh` CLI to find or open its pull request.
The clean branch deliberately excludes the conversation's internal step DAG and
`.caos` metadata.

API responses currently arrive one completed model round at a time. The
backend also does not yet provide reliable cancellation for a running turn;
the UI states both limitations rather than simulating them client-side.
