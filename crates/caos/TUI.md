# `caos tui`

`caos tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface keeps independent virtual conversations in a left sidebar. Each
two-row entry shows its title and latest user or agent message without exposing
the internal conversation ID. Each conversation has its own durable history,
multiline prompt, live activity, completed-turn hashes, and workspace diff.
Turns continue running when another conversation is selected, so several agent
workspaces can advance concurrently without touching the working checkout.

## Build and run

Run the client from a Git working tree whose `caos` remote points at a running
CAOS server:

```bash
git remote add caos http://localhost:9090
nix build
./result/bin/caos tui
```

During development, launch it with `cargo run -p caos --bin caos-cli -- tui`.

```text
caos tui                  continue the most recent conversation
caos tui --user alice     use alice's active conversation list
caos tui --new            start a fresh conversation
caos tui --from 5ec3751   branch from a completed turn
caos tui --tools tools/   add the tracked worker tools in tools/
caos tui --list-archived  list archived conversation IDs and titles
caos tui --unarchive ID   restore one conversation to the active list
```

`--user` defaults to `$USER`. Active and archived membership is stored on the
CAOS server under `refs/caos/users/<user>/conversations/{active,archived}/`,
not in local TUI state. Existing local conversation refs are imported the first
time that user opens the TUI.

## Worker tools

`--tools <source>` selects a worker-tool set for every conversation opened in
that TUI session. The source can be a Git-tracked directory, a
`/cas/std/<name>` tool set, or a 40-character tool-set tree hash. It has one
direct child per model-facing tool:

```text
tools/
└── example/
    ├── image
    └── tool.json
```

`tool.json` is the same tool definition passed to the Anthropic API:

```json
{
  "name": "example",
  "description": "What the tool does.",
  "input_schema": {"type": "object", "properties": {}}
}
```

The directory name must match `name`. `image` is the Caos-only executor
binding and names an already-runnable worker: `/cas/std/<name>`, a 40-character
Git image or curry hash, or a `docker://` reference. Tool names use ASCII
letters, digits, `_`, or `-` and cannot replace the harness's always-available
tools.

Every selected worker receives the model call's input object directly as the
run input, with execution context passed separately:

```text
/cas/args/in          model-supplied JSON blob
/cas/args/workspace/  current workspace tree
```

The workspace is not part of `tool.json` or `input_schema`. A worker returns a
tree containing `result.json`:

```json
{"content":"text returned to the model","is_error":false}
```

An optional `workspace/` in the result advances the conversation workspace;
without one, the workspace is unchanged. A returned workspace may not contain
the harness-reserved top-level `.caos` entry. `Ctrl+Shift+T` shows both these
selected workers and the workspace's dynamic `caos-tools/*.sh` tools.

## Controls

| Input | Action |
|---|---|
| `Enter` | Send the prompt |
| `Alt+Enter` or `Ctrl+J` | Insert a newline |
| `Tab` | Complete the selected slash command |
| `Up` / `Down` | Select a visible slash-command match |
| `Escape` | Dismiss slash-command matches |
| `Alt+Left` / `Alt+Right` or `Alt+B` / `Alt+F` | Move by whitespace-delimited words |
| `Alt+Backspace` / `Alt+Delete` | Delete the previous or next word |
| `Ctrl+Up` / `Ctrl+Down` | Select the previous or next conversation |
| `Ctrl+N` | Start a new virtual conversation |
| `Ctrl+W` | Archive the selected conversation for this user |
| `Ctrl+Q` | Switch between conversation and workspace changes |
| `Ctrl+T` or `Ctrl+A` | Enter or leave the Activity browser |
| `Ctrl+Shift+T` | Show the tools available to the selected conversation |
| `Up` / `Down` in Activity | Select the previous or next activity entry |
| `PageUp` / `PageDown` in Activity | Scroll the selected activity's full details |
| `Escape` in Activity | Return to the conversation |
| `PageUp` / `PageDown` in conversation | Scroll by rendered rows |
| Mouse wheel over the transcript | Scroll the conversation by rendered rows |
| Mouse wheel over Activity | Scroll the selected activity's full details |
| Mouse drag over visible transcript text | Select and copy rendered text |
| `Ctrl+Y` | Release mouse capture and freeze redraws for native selection |
| `Ctrl+L` twice | Load the selected conversation into the working tree |
| `Ctrl+P` twice | Push the selected conversation as a clean branch and open a PR |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Clear a non-empty prompt; exit when the prompt is empty |

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Enter `/title <new title>` to change the shared title without changing the
conversation ID or HEAD. Activity entries show the durable hashes of internal
harness steps for inspection; those step trees contain harness metadata and are
not branch points.

Conversation text renders `**bold**` and `_italic_` emphasis. Unmatched markers
remain visible, and marker-like text inside inline backticks is left literal.

A fresh conversation starts with a temporary `talk-N` title. Its first prompt
automatically becomes a whitespace-collapsed title of at most 60 characters.
Using `/title` before the first prompt keeps that explicit title instead.
Existing conversations are never automatically retitled.

Typing `/` at the start of the prompt shows matching slash commands and their
usage. Matches are case-sensitive. Use Up and Down to choose a match, then Tab
or Enter to complete it with a trailing space. Typing arguments closes the
menu. Escape dismisses it without changing the prompt. An unrecognized
slash-prefixed prompt is sent normally.

Bracketed paste mode keeps pasted newlines inside the prompt instead of
submitting partial lines. Pastes over 1,000 characters are kept out of the
editable buffer and shown as an atomic `[Pasted text: N chars]` placeholder.
The full text, including newlines, replaces the placeholder when the prompt is
sent. Backspace or Delete removes the whole placeholder. Press `Ctrl+C` to
clear a draft and any stored paste content; press it again on the empty prompt
to exit.

While a turn is running, a compact Activity row beneath the transcript shows a
verb such as `Thinking…`, `Reading…`, or `Running…` and the current operation.
`Ctrl+T` opens a focused Activity browser in all space above the composer;
`Ctrl+A` remains an alias. Up and Down select durable harness steps, and the
pane beside the list shows the selected step's complete result. Scroll long
results with PageUp, PageDown, or the mouse wheel. Escape or `Ctrl+T` returns
to the conversation. If the selection is already on the newest step, new
activity remains selected. Moving to an older step pauses that tail-follow
behavior.

Archiving atomically moves only the selected user's membership ref from
`active` to `archived`; it does not move the conversation HEAD or affect other
users. A running or publishing conversation must finish first. Closing an
unsent virtual conversation simply discards it. Use `--list-archived` and
`--unarchive <conversation-id>` outside the full-screen UI to recover old
conversations.

The transcript fills the conversation pane above the fixed composer. Use
`PageUp`, `PageDown`, or the mouse wheel over the transcript to scroll it.
Scrolling up pauses tail-follow until the viewport returns to the bottom.

Mouse-wheel routing requires terminal mouse capture, so CAOS implements visible
selection for the current transcript viewport. Drag across rendered transcript
text to highlight it and copy automatically on mouse release. macOS uses
`pbcopy`; other environments receive the same text through the standard OSC 52
terminal clipboard sequence.

For native terminal selection, press `Ctrl+Y`. CAOS releases mouse capture and
freezes redraws, so dragging and the terminal's normal copy shortcut (`Cmd+C`
on macOS or usually `Ctrl+Shift+C` elsewhere) work without moving output.
Press `Ctrl+Y` or `Escape` to resume.

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
