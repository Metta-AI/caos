# Shortcomings of what `caos mcp hook` records

Found while building and running `caos-conversation` against a real recorded
session (92 commits). What `caos-conversation` can show is limited by these; they
are candidates for fixing in `rust/crates/caos-cli/src/mcp/`.

1. **The model's text between tool calls is not recorded.** Only the user's
   prompts, calls to `mcp__caos__*` tools, and the last assistant message of each
   turn are. Progress remarks, plans and reasoning are missing, so a reader sees
   what was tried but not why.
2. **Parallel tool calls are recorded as a sequence.** Three calls issued in one
   assistant message (`ls`, `ls`, `grep`) appear as rounds 0, 1 and 2, each its
   own message, so it is impossible to see that they were issued together.
3. **A call that never reached caos leaves no trace.** In the sample, a call to a
   tool that does not exist (`mcp__caos__bash`, "No such tool available") is
   absent, as are the non-caos tools (`ToolSearch` to load schemas, `Skill`,
   task tools). All 33 recorded calls read `complete`, which makes the session
   look smoother than it was. Failed lookups and schema-loading detours are the
   sort of thing worth reviewing.
4. **Tool results are only in commit messages.** The tip's tree has the
   transcript and call arguments, but not results. Each result is a `payload`
   event whose bytes are a JSON array of numbers, so reading one takes a walk of
   the whole first-parent history (one `get-hash` per commit) and a decode step.
5. **No timings.** Every commit in the sample has the same timestamp, so there
   are no durations and no way to spot a slow call or a long wait.
6. **The model is recorded as `claude-code`.** No model name or version, so
   sessions cannot be compared by model.
7. **A closing message can be dropped.** `on_stop` skips the final assistant
   message when a call is still running (it was moved to the background), so a
   turn can end with no closing text recorded (from reading `mcp/mod.rs`; not
   seen in the sample).
8. **Harness context is not recorded.** The system reminders and hook output the
   model saw with each prompt are not in the transcript, so a reader cannot tell
   what instructions the model actually had.
9. **A tool-server restart can leave the conversation without an active
   request.** Mid-session the MCP server restarted, the next user prompt was not
   recorded as a request, and every caos tool call then failed with "no active
   request ... a tool call arrived before this session's prompt" until a later
   prompt was recorded. The model cannot recover from this by itself.
