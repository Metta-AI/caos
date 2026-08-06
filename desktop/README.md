# CAOS desktop

`caos-desktop` is a native Tauri client for the CAOS agent harness. It is a
thin presentation layer over the same `caos::chat` engine used by `caos tui`.

The app is intentionally scoped to one Git worktree. Launch it from inside the
repository you want CAOS to operate on; the repository must have a `caos`
remote and the ordinary CAOS environment (including `ANTHROPIC_API_KEY`).

```bash
cargo run --manifest-path desktop/src-tauri/Cargo.toml
```

Set `CAOS_REPO` to launch against a different worktree without changing the
shell's current directory. `CAOS_USER` overrides the active-conversation user;
otherwise the app uses `$USER` (or `$USERNAME` on Windows).

The desktop package is its own Cargo workspace. Tauri and WebView dependencies
therefore do not enter the core CAOS workspace, its lockfile, or worker images.

## Current scope

- repo-scoped active conversation list
- one reusable “New conversation” draft created with local-only Git work until
  its first prompt is sent
- live chat turns using the existing harness
- inline, collapsible tool activity and intermediate responses reconstructed
  from durable conversation history after reloads and restarts
- accumulated workspace diff in a conditional, resizable Changes inspector
- bottom-pinned composer with a separately scrolling transcript
- safe GitHub-style Markdown rendering for headings, lists, links, images,
  blockquotes, inline and fenced code, tables, and text emphasis
- a persistent, pointer- and keyboard-resizable sidebar with native macOS
  vibrancy and a heavily smudged, opaque frost layer
- visible repository and conversation loading states during startup
- TUI-style keyboard controls for sending, editing, switching conversations,
  changing views, reloading, and opening shortcut help with `Ctrl+H`
- direct navigation to the first nine visible conversations with `Ctrl+1`
  through `Ctrl+9`
- a clickable command palette that also opens with `Ctrl+Shift+P` or `/commands`
- persisted conversation renaming with `/rename <title>` or the TUI-compatible
  `/title <title>` alias
- persistent whole-interface zoom with `Cmd++`, `Cmd+-`, and `Cmd+0`

Publishing, checkout, archive management, and the TUI's other local commands
remain available through the existing CLI/TUI and are deliberately not
duplicated as desktop buttons yet.
