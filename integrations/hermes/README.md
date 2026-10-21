# hermes

caos, wired to Hermes Agent (nousresearch.com/hermes-agent) — in one
direction so far: **caos AS Hermes's tools.** Hermes's built-in
terminal/file/code-execution toolsets are banned (`config.yaml`'s
`agent.disabled_toolsets`), caos's MCP server (`caos mcp serve`) supplies
their replacements, and a Hermes plugin (`plugins/caos-hooks/`) replicates
what Claude Code's `UserPromptSubmit`/`PreToolUse`/`Stop` hooks do for the
`claude-code/` integration — see `rust/crates/caos-cli/MCP.md` for the
protocol both integrations share.

```
distribution.yaml           manifest: name, version, env_requires (CAOS_SERVER, CAOS_BIN)
config.yaml                 disabled_toolsets, mcp_servers.caos, plugins.enabled
bin/caos-mcp-launch         resolves/builds the caos binary, execs `caos mcp serve`
plugins/caos-hooks/         pre_tool_call / pre_llm_call / post_llm_call hooks
```

## Install

This directory IS a Hermes profile distribution root (`hermes profile
install` needs `distribution.yaml` at the root of whatever you point it at
— a git URL's repo root, or, for a local path, any directory, nested or
not). From a local caos checkout:

```bash
hermes profile install ~/path/to/caos/integrations/hermes --name caos-local --force -y
# fill in CAOS_SERVER in ~/.hermes/profiles/caos-local/.env
hermes -p caos-local chat
```

`hermes profile update caos-local` re-reads the same local path later —
picks up whatever this directory's `HEAD` currently is, same as
`integrations/claude-code/run` does implicitly by being launched from
inside the checkout.

## What's different from `claude-code/`

- **No flag-passing equivalent.** Claude Code's `cli/run` passes
  `--settings`/`--mcp-config` at launch, writing nothing to disk. Hermes has
  no per-invocation MCP override, so `hermes profile install --force`
  (re-)writes a real profile each time instead.
- **PreToolUse needs no subprocess.** Hermes's `pre_tool_call` hook can
  merge `caos_session`/`caos_tool_use_id`/`caos_prompt_id` into the call's
  arguments directly in Python (`{"action": "modify", "args": {...}}`) —
  the whole job `on_pre_tool_use()` does in `mod.rs`, without needing a
  round trip through the Rust binary for it.
- **UserPromptSubmit/Stop do need one.** Conversation creation and message
  recording are real git transitions, implemented only in the Rust client,
  so `pre_llm_call`/`post_llm_call` shell out to `caos mcp hook` with a
  payload shaped like Claude Code's own hook events.

## Not yet done

- **StopFailure.** `post_llm_call` doesn't fire on an interrupted/errored
  turn and Hermes has no exact equivalent in its hook catalog yet. Not a
  correctness gap — `settle.rs`'s `settle()` (in the step) already closes a request a
  dead session left open, the same self-healing a killed Claude Code
  process relies on — just a messier log on a crash than a clean `Stop`
  would leave.
- **Unverified: `${HERMES_HOME}` substitution in `mcp_servers.caos.command`.**
  Confirmed-working: `${VAR}` substitution in `config.yaml`'s `env:` values.
  Not yet confirmed: whether `$HERMES_HOME` is a real exported env var at
  the point `command:` itself gets resolved, vs. an internal-only
  `get_hermes_home()` helper. `hermes mcp test caos` after a real install is
  the fast way to check; if it fails to resolve, hardcode the profile's
  absolute path instead (loses portability across machines, but is correct
  immediately).
- **Claude Code's `/fork-caos-conversation`
  slash command** has no Hermes equivalent yet — picking up a conversation
  by its commit hash in a new Hermes session needs the same verb,
  wired as a Hermes slash command or just plain prompted instructions for now.
