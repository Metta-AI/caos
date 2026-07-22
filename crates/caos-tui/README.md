# caos-tui

`caos-tui` is a full-screen terminal client for the CAOS agent harness. It uses
the same conversation engine as `caos talk`, while keeping terminal UI
dependencies out of the worker-side `caos` binary.

The interface presents one active conversation with durable history,
multiline prompts, live status and tool activity, completed-turn hashes, and
workspace diff review.

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

## Controls

| Input | Action |
|---|---|
| `Enter` | Send the prompt |
| `Alt+Enter` or `Ctrl+J` | Insert a newline |
| `F2` | Switch between conversation and workspace diff |
| `F3` | Expand or collapse live Activity above the prompt |
| `PageUp` / `PageDown` | Scroll by rendered rows |
| Mouse wheel | Scroll by rendered rows |
| `Ctrl+A` twice | Apply the conversation workspace diff |
| `Ctrl+R` | Reload completed conversation history |
| `Ctrl+C` | Exit |

Completed user and agent turns show branchable hashes in the transcript. Enter
`/from <turn-hash>` to start a fresh conversation from one without leaving the
TUI. Activity entries show the durable hashes of internal harness steps for
inspection; those step trees contain harness metadata and are not branch
points.

## Workspace safety

Agent workspaces remain virtual commit trees. Opening the TUI never overwrites
the working checkout. Applying changes requires two `Ctrl+A` presses, a clean
working tree, and a successful `git apply --check` before the patch is applied.

API responses currently arrive one completed model round at a time. The
backend also does not yet provide reliable cancellation for a running turn;
the UI states both limitations rather than simulating them client-side.
