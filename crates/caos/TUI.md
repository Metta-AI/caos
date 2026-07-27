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
caos tui --list-archived  list archived conversation IDs and titles
caos tui --unarchive ID   restore one conversation to the active list
```

`--user` defaults to `$USER`. Active and archived membership is stored on the
CAOS server under `refs/caos/users/<user>/conversations/{active,archived}/`,
not in local TUI state. Existing local conversation refs are imported the first
time that user opens the TUI.

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
| `Ctrl+T` | Show the tools available to the selected conversation |
| `Ctrl+A` | Expand or collapse live Activity above the prompt |
| `PageUp` / `PageDown` | Scroll by rendered rows |
| Mouse drag | Select text using the terminal's native selection |
| `Ctrl+Y` | Freeze or resume redraws while selecting text |
| `Ctrl+L` twice | Load the selected conversation into the working tree |
| `Ctrl+P` twice | Push the selected conversation as a clean branch and open a PR |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Exit |

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Enter `/title <new title>` to change the shared title without changing the
conversation ID or HEAD. Activity entries show the durable hashes of internal
harness steps for inspection; those step trees contain harness metadata and are
not branch points.

A fresh conversation starts with a temporary `talk-N` title. Its first prompt
automatically becomes a whitespace-collapsed title of at most 60 characters.
Using `/title` before the first prompt keeps that explicit title instead.
Existing conversations are never automatically retitled.

Typing `/` at the start of the prompt shows matching slash commands and their
usage. Matches are case-sensitive. Use Up and Down to choose a match, then Tab
or Enter to complete it with a trailing space. Typing arguments closes the
menu. Escape dismisses it without changing the prompt. An unrecognized
slash-prefixed prompt is sent normally.

Archiving atomically moves only the selected user's membership ref from
`active` to `archived`; it does not move the conversation HEAD or affect other
users. A running or publishing conversation must finish first. Closing an
unsent virtual conversation simply discards it. Use `--list-archived` and
`--unarchive <conversation-id>` outside the full-screen UI to recover old
conversations.

Mouse capture is disabled, so dragging across visible text uses the terminal's
native selection without entering a special mode. Use the terminal's normal
copy shortcut (`Cmd+C` on macOS or usually `Ctrl+Shift+C` elsewhere).
If a conversation is still producing output, press `Ctrl+Y` first to freeze
redraws and keep the selection stable, then press `Ctrl+Y` or `Escape` to
resume the live interface. Use `PageUp` and `PageDown` for in-app scrolling;
mouse-wheel scrolling is intentionally left to the terminal.

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
