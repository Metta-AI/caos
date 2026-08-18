# CAOS desktop

`caos-desktop` is a native Tauri client for the CAOS agent harness. It is a
thin presentation layer over the same `caos::chat` engine used by `caos tui`.

The app is intentionally scoped to one Git worktree. Launch it from inside the
repository you want CAOS to operate on; the repository must have a `caos`
remote and the ordinary CAOS environment (including `ANTHROPIC_API_KEY`).

```bash
nix develop
npm ci --prefix desktop
npm run --prefix desktop dev
npm run --prefix desktop dev -- --username "Alice Smith" --model claude-opus-5
```

The frontend build bundles the maintained Markdown, syntax-highlighting, and
diff-parsing libraries into the static assets embedded by Tauri. Nix performs
that build automatically; `npm run --prefix desktop dev` rebuilds it before a
local Cargo launch. The development shell supplies Node.js and the bundler.

The reproducible Nix build is a separate flake output so it stays out of the
core workspace and worker images:

```bash
nix build .#caos-desktop
./result/bin/caos-desktop
nix run .#caos-desktop
nix run .#caos-desktop -- --username "Alice Smith" --model claude-opus-5
```

The package build runs the desktop Rust and JavaScript tests. It is also exposed
as `checks.<system>.caos-desktop`, so `nix flake check` validates the same
derivation without maintaining a second build path. Bare `nix build` continues
to build the CLI and daemon host tools; GUI dependencies remain opt-in.

Set `CAOS_REPO` to launch against a different worktree without changing the
shell's current directory. The desktop uses `$USER` as its conversation
identity; `--username <name>` overrides it explicitly.

The desktop package is its own Cargo workspace. Tauri and WebView dependencies
therefore do not enter the core CAOS workspace, its lockfile, or worker images.

## Current scope

- repo-scoped active conversation list
- reusable “New conversation” drafts based on the local default-branch tip,
  with `/from <commit>` for starting from another completed turn
- live chat turns using the existing harness
- a per-conversation model selector in the composer, initialized by the sole
  model launch option, `--model <model>`, plus the TUI-compatible `/model`
  command
- durable multiplayer conversations with peer attribution, `/invite`, and
  copyable `/ref` merge targets
- materialized `/from` forks and automatically discovered, parent-indented
  subagent conversations
- interjections into active turns, durable activity polling after restarts,
  and `Escape` interruption
- generated conversation titles with the first-prompt fallback used by the TUI
- inline, collapsible tool activity and intermediate responses reconstructed
  from durable conversation history after reloads and restarts
- accumulated workspace diff in a conditional, resizable Changes inspector
- clean-checkout loading with `Ctrl+L`, and working-tree updates through
  `/update-tree <message>`
- pull-request publishing through the same ordinary merge-turn workflow as the
  TUI
- conversation archiving and restoration
- the project tool-set inspector available with `Ctrl+Shift+T`
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
- slash-command completion in the composer
- persisted conversation renaming with `/rename <title>` or the TUI-compatible
  `/title <title>` alias
- persistent whole-interface zoom with `Cmd++`, `Cmd+-`, and `Cmd+0`
