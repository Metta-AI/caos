//! Recording a Claude Code session as an ordinary CAOS conversation.
//!
//! Claude Code drives the model; CAOS keeps the durable log. Every hook Claude
//! Code fires arrives here as one JSON object on stdin, so the whole surface is
//! a single `caos mcp hook` command and `.claude/settings.json` needs no shell
//! at all — no `jq`, no quoting, none of the constructs CLAUDE.md catalogs as
//! this tree's most reliable source of bugs. The payload names its own event
//! (`hook_event_name`), so one command serves every hook.
//!
//! The conversation these events build is an ordinary one: the same head ref,
//! the same append-only spine, the same transitions `caos tui` already replays
//! (design/chat.md). Its id is derived from the Claude Code session id rather
//! than stored in a side table, so there is no local state to lose or corrupt:
//! the ref is the whole record.
//!
//! AND THE TOOLS ARE ORDINARY TOOLS. Nothing here implements one. A call is
//! one request to the server: `llm-step` in its tools-only mode, given the call,
//! declares it as the model's own and runs it exactly as it runs a call in a
//! turn it drives itself, and answers with what the tool said — so `caos mcp`
//! offers whatever that step offers, in whatever
//! repository it is pointed at, and a tool reads one way wherever a model meets
//! it. The step is named by `--llm-step`, as `tui` and `chat` name it.
//!
//! What Claude Code keeps is the MODEL. It chose the call and will read the
//! result; the request a prompt admits is claimed by the step in the same
//! write, because the turn is already running by the time the hook fires.

mod diag;
mod resume;
mod serve;
mod session_base;
mod workspace;

use std::io::Read;

use serde_json::{json, Value};

use caos::{GitTransport, Transport};
use conversation_protocol::v3::oid::Oid;
use conversation_protocol::v3::refs;

use crate::{
    conversation_ref, eval_base, fetch_head, oid, open_store, resolve_username, seed_content,
    TurnOptions, LLM_STEP_ARG,
};

/// Conversation ids for recorded sessions live under one component so they are
/// obvious in the sidebar and cannot collide with a hand-named conversation.
/// `ConversationId::parse` accepts it: Claude Code session ids are lowercase
/// hex and dashes.
const SESSION_PREFIX: &str = "cc/";

/// Claude Code names an MCP tool `mcp__<server>__<tool>`. Only calls to our own
/// server get a session injected; everything else this hook sees is left
/// exactly as the model wrote it.
const TOOL_PREFIX: &str = "mcp__caos__";

/// What a request record says ran the turn. Claude Code chooses the model and
/// does not tell a hook which, so naming the harness is the honest answer --
/// better than a plausible-looking model string nothing verified.
const CLAUDE_CODE_MODEL: &str = "claude-code";

/// Exposed for `caos-cli.rs`'s own `cc_transport()`: a server-wide scratch
/// checkout (see `session_base`'s module doc comment) for the callers that
/// run before any session is known at all -- `tools/list`, `caos mcp warm`,
/// `caos mcp diag`. `None` when `CAOS_MCP_DEFAULT_SERVER` is not set, so a
/// caller's existing cwd/`$CLAUDE_PROJECT_DIR`-not-a-repo error is unchanged.
pub fn default_server_transport() -> Option<Result<GitTransport, String>> {
    session_base::default_server_scratch()
}

/// The workspace arrives UNRESOLVED because only `serve` can carry on without
/// one. A hook that cannot find the repository has nothing to record into and
/// should say so; a tool server that cannot find it still has to answer, or
/// the session sees `CONNECTION_CLOSED` and no reason at all.
///
/// `--llm-step:<type>=<value>` names the step that runs the tools, exactly as
/// `tui` and `chat` take it: a session in another repository points it at
/// wherever that repository mounted caos.
pub fn cli_mcp(workspace: Result<GitTransport, String>, args: &[String]) -> Result<(), String> {
    let mut options = TurnOptions::default();
    let mut rest = Vec::new();
    for argument in args {
        // `--base=<full sha>` SEEDS THE CONVERSATION from a commit other than
        // HEAD, and resolves `--llm-step` in that commit's tree rather than
        // the workspace (`step_image`). Dev mode needs both: `bootstrap.go`
        // mints a commit whose `.caos-expr` points at the dev stack and leaves
        // the checkout clean. Measured before this existed: a session ran the
        // dev client and dev step while every `caos-std/<entry>` still
        // resolved through the committed pin.
        //
        // A FULL SHA AND NOTHING ELSE. A revspec here would reintroduce
        // "resolved through a moving head" in a place nobody would think to
        // look -- the same pairing `install.sh` refuses `--base` a branch for.
        // The caller mints an unreferenced commit (`git commit-tree`), so there
        // is no name for this to accept anyway.
        if let Some(sha) = argument.strip_prefix("--base=") {
            if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "--base must be a full 40-character commit sha, not {sha:?}"
                ));
            }
            options.base = Some(sha.to_string());
            continue;
        }
        if !options.take_image_arg(argument) {
            rest.push(argument.as_str());
        }
    }
    match rest.first().copied() {
        Some("hook") => hook(workspace, &options),
        // Needs no workspace: it exists to describe one that is missing.
        Some("diag") => diag::cli(&rest[1..]),
        Some("serve") => serve::serve(workspace, options),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    format!(
        "usage:\n  \
         caos mcp hook    (reads one Claude Code hook payload on stdin)\n  \
         caos mcp serve   (workspace tool server; JSON-RPC on stdio)\n\n\
         Both name the step that runs this session's tools:\n  \
         {}",
        crate::missing_image_arg(LLM_STEP_ARG)
    )
}

/// What one tool call answers with: the observation the transcript kept, and
/// whether the tool itself failed. A failed tool is a RESULT, not an error --
/// the model is meant to read it and react.
struct ToolOutcome {
    text: String,
    is_error: bool,
}

/// A tool call, ready to send: the session's workspace, the conversation it
/// belongs to, and the parameters of the request that carries it.
struct PreparedCall {
    workspace: GitTransport,
    conversation: String,
    params: Vec<(&'static str, String)>,
}

impl PreparedCall {
    fn params(&self) -> Vec<(&str, &str)> {
        self.params
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect()
    }
}

/// Everything a tool call needs before it is sent: open the session's
/// workspace, find the conversation, and form the step's parameters.
///
/// A FRESH transport, not the one `serve` opened at spawn. That one was read
/// once, and in a cloud session it can predate the SessionStart hook that adds the
/// `caos` remote -- so a call driven through it dials a server it does not know
/// and fails "no `caos` git remote" while the remote is right there.
///
/// The conversation must already exist. The prompt hook opens it, and runs in
/// another process, so the first call can beat it; `wait_for_conversation` covers
/// that race.
fn prepare_call(session: &str, name: &str, args: &Value) -> Result<PreparedCall, String> {
    let workspace = workspace::open(session)
        .map_err(|error| format!("cannot open the caos workspace for this call: {error}"))?;
    let conversation = resume::conversation_for_session(&workspace, session)?;
    wait_for_conversation(&workspace, &conversation);
    let call = args
        .get("caos_tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_string();
    let arguments = declared_args(args).to_string();
    Ok(PreparedCall {
        workspace,
        conversation,
        params: vec![
            ("tools-only", call),
            ("tool", name.to_string()),
            ("arguments", arguments),
        ],
    })
}

/// Run one tool and wait for what it said: one request to the server.
///
/// The request is `llm-step` in its tools-only mode, given the call -- its id,
/// the tool and its arguments -- and the conversation it belongs to. The step
/// declares the call on the conversation's active request, runs it, and answers
/// with the call's record, so this process neither writes the conversation nor
/// reads it back: it sends a call and receives what the tool said. `name` is the
/// STEP's name for the tool (`run_tool`, `tool_help`, `eval_path`); the server
/// presents them as `run`, `help` and `eval-path`.
fn run_tool(
    _serve_transport: &GitTransport,
    options: &TurnOptions,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<ToolOutcome, String> {
    let call = prepare_call(session, name, args)?;
    let (kind, result) = run_step(&call.workspace, options, &call.conversation, &call.params())?;
    decode_result(&call.workspace, &kind, &result)
}

/// Start a tool without waiting, and return the ticket to ask for it by. The same
/// call as [`run_tool`], begun with `POST /start/run`, so a tool that outlasts
/// what a client will wait for is not lost with its connection.
fn start_tool(
    options: &TurnOptions,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let call = prepare_call(session, name, args)?;
    start_step(&call.workspace, options, &call.conversation, &call.params())
}

/// The state of a started tool, from its ticket.
fn tool_result(session: &str, ticket: &str) -> Result<ToolOutcome, String> {
    if ticket.len() != 40 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{ticket:?} is not a ticket; `start` returns one"));
    }
    let workspace = workspace::open(session)
        .map_err(|error| format!("cannot open the caos workspace for this call: {error}"))?;
    wait_server_reachable(&workspace)?;
    Ok(match caos::line_result(&workspace.server_url()?, ticket)? {
        caos::LineResult::Pending => ToolOutcome {
            text: "pending: the call has not finished. Ask for `result` again with this \
                       ticket."
                .to_string(),
            is_error: false,
        },
        caos::LineResult::Done { kind, hash } => decode_result(&workspace, &kind, &hash)?,
        caos::LineResult::Failed { status, message } => ToolOutcome {
            text: format!("the call failed ({status}): {message}"),
            is_error: true,
        },
    })
}

/// What a tools-only run answered, as the model reads it: the call's `status` and
/// `observation`, which are the same bytes the conversation recorded.
fn decode_result(t: &GitTransport, kind: &str, result: &str) -> Result<ToolOutcome, String> {
    if kind != "tree" {
        return Err(format!("the step answered a {kind}, not a tool result"));
    }
    let files = caos::tree_blobs(t, result)?;
    let part = |file: &str| {
        files
            .get(file)
            .ok_or_else(|| format!("the step's result {result} has no {file}"))
    };
    let status = String::from_utf8_lossy(part("status")?).into_owned();
    let (is_error, text) = crate::tool_outcome(status.trim(), part("observation")?)?;
    Ok(ToolOutcome { text, is_error })
}

/// A request to run the step, ready to send: where, with what parameters, and
/// presenting which secrets.
struct StepRequest {
    server: String,
    params: Vec<(String, String)>,
    secrets: caos::Secrets,
}

impl StepRequest {
    fn params(&self) -> Vec<(&str, &str)> {
        self.params
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }
}

/// The request that runs the step on a conversation, ready to send: the server,
/// the parameters (`params` are the step's own arguments for this call -- a tool
/// call, a recorded event -- and the step and the conversation are added), and
/// the secrets to present.
///
/// EVERYTHING a tool call or a hook does to a conversation goes through here, so
/// that is the whole of the client's part: no checkout of the conversation is
/// read, none is written, and nothing is pushed first.
fn step_request(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    params: &[(&str, &str)],
) -> Result<StepRequest, String> {
    let secrets = caos::Secrets::current().for_conversation(id);
    // REACHABLE FIRST, in front of everything below. Resolving the step talks
    // to the server, and in a cloud session that server is reached through a
    // tunnel whose far end, when gone, accepts and swallows: the wait is
    // unbounded, and this is the path a HOOK takes. A `UserPromptSubmit` hook
    // that never returns takes the session's prompt with it, which presents as
    // Claude Code hanging on the first thing you type, with nothing saying why.
    wait_server_reachable(t)?;
    let step = step_image(t, options, &secrets)?;
    let mut all: Vec<(String, String)> = vec![
        ("base:hash".to_string(), step),
        ("conversation".to_string(), id.to_string()),
    ];
    all.extend(
        params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    let salt = std::env::var("CAOS_SALT").unwrap_or_default();
    if !salt.is_empty() {
        all.push(("salt".to_string(), salt));
    }
    Ok(StepRequest {
        server: t.server_url()?,
        params: all,
        secrets,
    })
}

/// Run the step on a conversation and wait: the result's `(kind, hash)`.
fn run_step(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    params: &[(&str, &str)],
) -> Result<(String, String), String> {
    let request = step_request(t, options, id, params)?;
    let all = request.params();
    caos::eval_line_request(&request.server, "run", &all, &request.secrets)
}

/// Start the step on a conversation without waiting: the ticket.
fn start_step(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    params: &[(&str, &str)],
) -> Result<String, String> {
    let request = step_request(t, options, id, params)?;
    let all = request.params();
    caos::start_line_request(&request.server, "run", &all, &request.secrets)
}

/// The step, resolved from the tree the conversation seeds from: `--base`'s
/// when given, the workspace otherwise.
///
/// ONE TREE FOR BOTH. Dev mode's `--base` is a commit whose `.caos-expr` points
/// at the dev stack, and nothing else carries that pin -- the checkout is left
/// as it was cloned. Resolved from the workspace instead, the session would run
/// a dev client against the committed tools.
fn step_image(
    t: &GitTransport,
    options: &TurnOptions,
    secrets: &caos::Secrets,
) -> Result<String, String> {
    let tree = match &options.base {
        Some(base) => Some(
            t.git_capture(
                &["rev-parse", "--verify", &format!("{base}^{{tree}}")],
                None,
            )
            .map_err(|e| format!("reading --base {base}: {e}"))?
            .trim()
            .to_string(),
        ),
        None => None,
    };
    crate::resolve_image_arg(
        t,
        options.llm_step.as_deref(),
        LLM_STEP_ARG,
        secrets,
        tree.as_deref(),
    )
}

/// Wait, bounded, for the caos server to answer -- the hook's counterpart to
/// the serve resolver's retry loop.
///
/// The budget is a compromise between the two ways this hurts. Too short and
/// the first prompt races tunnel bringup and loses (the bug this exists for);
/// too long and a genuinely-down server hangs the prompt, since a
/// `UserPromptSubmit` hook blocks the turn until it returns. Each attempt is
/// `ensure_server_reachable`'s own 5s round trip, so `HOOK_REACH_ATTEMPTS`
/// probes plus the interstitial sleeps is the ceiling -- ~35s, which outlasts
/// tunnel bringup. The common case returns on the first probe.
///
/// THE HOOK'S OWN TIMEOUT IS SET, in `shared/settings.json`, because Claude
/// Code's 60s default is not enough over iroh: this wait plus a resume's fetch,
/// settle and push plus `record_prompt`'s pushes exceeded it in a cloud
/// session, and a killed hook lets the prompt through with no conversation and
/// no reason given.
const HOOK_REACH_ATTEMPTS: u32 = 6;
const HOOK_REACH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) fn wait_server_reachable(t: &GitTransport) -> Result<(), String> {
    let mut last = t.ensure_server_reachable();
    let mut attempt = 1;
    while last.is_err() && attempt < HOOK_REACH_ATTEMPTS {
        std::thread::sleep(HOOK_REACH_INTERVAL);
        last = t.ensure_server_reachable();
        attempt += 1;
    }
    last
}

/// How long a tool call waits for its conversation to exist before proceeding
/// to the append that would refuse it.
///
/// The conversation is created by the `UserPromptSubmit` hook, a SEPARATE
/// process, and Claude Code does not hold the turn for it -- so the model's
/// first tool call can arrive before the record lands and would otherwise be
/// refused ("no conversation to record into"), failing a session's whole first
/// turn while every later turn works (by then the record exists). So a call
/// WAITS for the record the hook is still writing.
///
/// In a cloud session the hook takes ~10s -- its curry, base push and
/// conversation push each cross the iroh tunnel -- so this is a short wait.
/// Probing is cheap: `fetch_head` returns early on an absent ref,
/// with no fetch and no local write. Bounded well past the hook's time; a
/// conversation that never appears still errors, just after the wait.
const TOOL_WAIT_ATTEMPTS: u32 = 20;
const TOOL_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

fn wait_for_conversation(t: &GitTransport, id: &str) {
    for attempt in 0..TOOL_WAIT_ATTEMPTS {
        if let Ok(store) = open_store(t) {
            if matches!(fetch_head(t, &store, id), Ok(Some(_))) {
                return;
            }
        }
        if attempt + 1 < TOOL_WAIT_ATTEMPTS {
            std::thread::sleep(TOOL_WAIT_INTERVAL);
        }
    }
}

/// The model's own arguments, without the values the hook injected: those are
/// plumbing, and showing them in the transcript would misrepresent the call the
/// model actually made.
fn declared_args(args: &Value) -> Value {
    let mut declared = args.clone();
    if let Some(object) = declared.as_object_mut() {
        object.remove("caos_session");
        object.remove("caos_tool_use_id");
        object.remove("caos_prompt_id");
    }
    declared
}

/// Dispatch one hook payload. An event we do not record is not an error: Claude
/// Code fires many, and a settings file that routes extra ones here should keep
/// working rather than failing a turn.
///
/// THE CHECKOUT COMES FROM THE SESSION'S RECORDED ENTRY (`workspace`), never
/// from wherever this process happens to stand. The one exception is a
/// session's first prompt, which has no entry yet and so uses the discovered
/// `workspace` and records it. Any other event with no entry fails, loudly:
/// diagnostics first (stderr and the diag journal), then exit 2 for the two
/// events where that is visible (a prompt is blocked with the reason shown, a
/// tool call is refused with it shown to the model). Elsewhere a failed hook is
/// shown only as an error line, and exit 2 on `Stop` would force the session on.
fn hook(discovered: Result<GitTransport, String>, options: &TurnOptions) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("reading hook payload: {error}"))?;
    let payload: Value =
        serde_json::from_str(&input).map_err(|error| format!("parsing hook payload: {error}"))?;
    let event = string_field(&payload, "hook_event_name")?;
    if !matches!(
        event,
        "UserPromptSubmit" | "PreToolUse" | "Stop" | "StopFailure"
    ) {
        return Ok(());
    }
    // Logged at the START too, so a hook killed mid-run (its process gone before
    // the end line) is distinguishable from one that never fired.
    debug_log_hook("start", event, &payload, None);
    let started = std::time::Instant::now();
    let session = string_field(&payload, "session_id")?;
    let first_prompt = event == "UserPromptSubmit" && !workspace::exists(session)?;
    let opened = if first_prompt {
        let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
        session_base::open_for_new_session(session, prompt, discovered)
            .and_then(|t| workspace::record(session, &t).map(|()| t))
    } else {
        workspace::open(session)
    };
    let t = match opened {
        Ok(t) => t,
        Err(error) => {
            diag::emit(&diag::Context {
                event,
                source: payload.get("source").and_then(Value::as_str),
                session: Some(session),
                reason: Some(&error),
            });
            if matches!(event, "UserPromptSubmit" | "PreToolUse") {
                eprintln!(
                    "caos: this session's workspace is unavailable, so caos did nothing: {error}"
                );
                std::process::exit(2);
            }
            return Err(error);
        }
    };
    let result = match event {
        "UserPromptSubmit" => on_user_prompt(&t, options, &payload),
        "PreToolUse" => on_pre_tool_use(&payload),
        "Stop" => on_stop(&t, options, &payload),
        _ => on_stop_failure(&t, options, &payload),
    };
    debug_log_hook("end", event, &payload, Some((started.elapsed(), &result)));
    result
}

/// The hook debug log, when `CAOS_CC_HOOK_LOG` names one -- OFF by default.
///
/// The hooks are the one part of this that runs in a process nobody watches:
/// Claude Code spawns them and keeps only a pass/fail. So when a session's
/// conversation is unexpectedly absent, or its first turn is slow, set this to a
/// path and every hook writes a start line, an end line (session, elapsed,
/// ok/ERR) and its `record_prompt` phase timings. `None` -- unset -- makes all
/// of `debug_log_hook`/`cc_timing` no-ops, so production writes nothing.
fn hook_log_path() -> Option<std::path::PathBuf> {
    match std::env::var("CAOS_CC_HOOK_LOG") {
        Ok(path) if !path.is_empty() => Some(std::path::PathBuf::from(path)),
        _ => None,
    }
}

/// Append one line to the hook debug log, if it is enabled.
fn append_hook_log(line: &str) {
    let Some(path) = hook_log_path() else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// One indented `timing <phase> <secs>` line under the hook's own, so a slow or
/// wedged hook shows which of `record_prompt`'s phases owns the time (or, by the
/// missing line, hangs). No-op unless the log is enabled.
fn cc_timing(phase: &str, elapsed: std::time::Duration) {
    append_hook_log(&format!("  timing {phase} {:.2}s\n", elapsed.as_secs_f64()));
}

/// A start/end line per hook invocation. The start line, logged before the work,
/// distinguishes a hook killed mid-run (start, no end) from one that never fired.
fn debug_log_hook(
    phase: &str,
    event: &str,
    payload: &Value,
    done: Option<(std::time::Duration, &Result<(), String>)>,
) {
    if hook_log_path().is_none() {
        return;
    }
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let prompt_len = payload
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    let tail = match done {
        None => String::new(),
        Some((elapsed, Ok(()))) => format!(" {:.1}s ok", elapsed.as_secs_f64()),
        Some((elapsed, Err(error))) => {
            format!(
                " {:.1}s ERR {}",
                elapsed.as_secs_f64(),
                error.replace('\n', " ")
            )
        }
    };
    append_hook_log(&format!(
        "{phase} {event} session={session} prompt_len={prompt_len}{tail}\n"
    ));
}

/// The user's prompt, and the only event allowed to create the conversation:
/// the first prompt of a session establishes its base and fallback title
/// exactly as the TUI's first message does.
/// EXIT 2, the one status with which Claude Code blocks a prompt and shows the
/// hook's stderr. Any other failure lets the prompt through and drops the
/// reason, and for a failed fork or resume that is a session running on with
/// no conversation behind it: the model, following the command's text, calls a
/// tool that answers `no conversation ... to record into`, and the actual
/// refusal (not the head, not on this server) is never seen by anyone.
fn refuse_prompt(error: &str) -> ! {
    eprintln!(
        "caos: this session is NOT attached to that conversation: {error}\n\
         Nothing was recorded; fix the command and send it again."
    );
    std::process::exit(2)
}

fn on_user_prompt(t: &GitTransport, options: &TurnOptions, payload: &Value) -> Result<(), String> {
    let prompt = string_field(payload, "prompt")?;
    if prompt.trim().is_empty() {
        return Ok(());
    }
    // `/fork-caos-conversation <hash>` and `/resume-caos-conversation <hash>`
    // decide WHICH conversation this session records into, so they run before it
    // is looked up. Both are no-ops once the session has its conversation.
    let command = || -> Result<Option<String>, String> {
        match resume::parse_command(prompt)? {
            Some(command) => {
                // Both talk to the server before anything below would have
                // proven it reachable.
                wait_server_reachable(t)?;
                resume::begin(t, string_field(payload, "session_id")?, &command)
            }
            None => Ok(None),
        }
    };
    let note = match command() {
        Ok(note) => note,
        Err(error) => refuse_prompt(&error),
    };
    let id = recorded_conversation(t, payload)?;
    let readers = crate::secret_store::reader_summary();
    if record_prompt(t, options, &id, prompt)? {
        announce(&format!(
            "caos conversation ref {}; {readers}",
            conversation_ref(&id)?
        ))?;
    } else if let Some(note) = note {
        announce(&format!("{note}; {readers}"))?;
    }
    Ok(())
}

/// What the step answered to a recorded event: which of its outcomes it was
/// (`created`, `recorded`, `joined`, `held`, `nothing`) and the head it left.
struct Recorded {
    status: String,
    head: String,
}

/// Send one event to the step (`llm-step --record=<kind>`) and read its answer.
///
/// The hooks used to write the conversation themselves, from the harness's
/// checkout. A hook is now one request: the step makes the same transitions on
/// the server, so this process needs no checkout of the conversation, writes
/// nothing, and a repeated event (a retried hook) names the message id it
/// already recorded and writes nothing twice.
fn record_event(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    kind: &str,
    extra: &[(&str, &str)],
) -> Result<Recorded, String> {
    let message_id = caos::fresh_entropy()?;
    let username = resolve_username(t, None)?;
    let mut params = vec![
        ("record", kind),
        ("message-id", message_id.as_str()),
        ("actor", username.as_str()),
    ];
    params.extend_from_slice(extra);
    let (answer, tree) = run_step(t, options, id, &params)?;
    if answer != "tree" {
        return Err(format!("the step answered a {answer}, not a record"));
    }
    let files = caos::tree_blobs(t, &tree)?;
    let text = |file: &str| {
        files
            .get(file)
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string())
            .ok_or_else(|| format!("the step's answer {tree} has no {file}"))
    };
    Ok(Recorded {
        status: text("status")?,
        head: text("head")?,
    })
}

/// Record a user's prompt, creating the conversation on the session's first one;
/// true when this prompt is the one that created it.
///
/// The step makes the message, the request it opens, and the claim that puts the
/// request in `running`, in one compare-and-swap. The claim is not a shortcut:
/// v3 requires it before any tool may be recorded, and Claude Code has already
/// begun the turn by the time this hook fires, so no runner will pick the
/// request up.
///
/// A NEW conversation needs its content, and that is the one thing the step
/// cannot find for itself: the base this hook stands in (or names) is resolved
/// and pushed here, and the step is handed its tree. A conversation that
/// exists needs none, so the base is not resolved for every prompt.
fn record_prompt(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    prompt: &str,
) -> Result<bool, String> {
    let phase = std::time::Instant::now();
    let exists = fetch_head(t, &open_store(t)?, id)?.is_some();
    cc_timing("fetch_head", phase.elapsed());
    let content = if exists {
        None
    } else {
        let phase = std::time::Instant::now();
        let content = content_tree(t, options, prompt)?;
        cc_timing("content_tree", phase.elapsed());
        Some(content.to_string())
    };
    let mut extra = vec![("text", prompt), ("model", CLAUDE_CODE_MODEL)];
    if let Some(content) = &content {
        extra.push(("content:hash", content));
    }
    let phase = std::time::Instant::now();
    let recorded = record_event(t, options, id, "prompt", &extra)?;
    cc_timing("record", phase.elapsed());
    Ok(recorded.status == "created")
}

/// The tree a new conversation starts with: the base commit's, on the server.
fn content_tree(t: &GitTransport, options: &TurnOptions, prompt: &str) -> Result<Oid, String> {
    let base = oid(
        &match &options.base {
            None if session_base::names_a_base(prompt) => {
                session_base::resolve_default_base(t, prompt)?
            }
            _ => eval_base(t, options)?,
        },
        "conversation base",
    )?;
    let mut store = open_store(t)?;
    seed_content(t, &mut store, &base)
}

/// Tell a caos workspace tool which conversation it is working in.
///
/// The tool server is spawned once per session and is otherwise stateless, so
/// this is the only thing that attributes a call to a conversation. Both values
/// are declared in every tool's schema rather than smuggled in, so the model's
/// own call stays schema-valid and this hook only fills in values the tool
/// already accepted.
///
/// No `permissionDecision` is emitted. Injection and permission are separate
/// concerns: allowing these tools without a prompt is a choice for
/// `permissions.allow`, not something a hook should decide on the user's behalf
/// just because it happened to be in the call path.
fn on_pre_tool_use(payload: &Value) -> Result<(), String> {
    let tool = string_field(payload, "tool_name")?;
    if !tool.starts_with(TOOL_PREFIX) {
        return Ok(());
    }
    let session = string_field(payload, "session_id")?;
    let mut input = payload
        .get("tool_input")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(object) = input.as_object_mut() else {
        return Err(format!("{tool} was called with a non-object tool_input"));
    };
    object.insert("caos_session".to_string(), json!(session));
    if let Some(id) = payload.get("tool_use_id").and_then(Value::as_str) {
        object.insert("caos_tool_use_id".to_string(), json!(id));
    }
    // The turn this call belongs to, which becomes the event's `request` — the
    // same job `llm-step`'s `run` does for a turn it drives itself.
    if let Some(prompt) = payload.get("prompt_id").and_then(Value::as_str) {
        object.insert("caos_prompt_id".to_string(), json!(prompt));
    }
    let response = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": input,
        }
    });
    println!(
        "{}",
        serde_json::to_string(&response)
            .map_err(|error| format!("encoding hook response: {error}"))?
    );
    Ok(())
}

/// The conversation a hook payload's session records into: the one it was
/// resumed onto (`resume`), or else the one its id derives.
fn recorded_conversation(t: &GitTransport, payload: &Value) -> Result<String, String> {
    resume::conversation_for_session(t, string_field(payload, "session_id")?)
}

/// A session's conversation id is derived, never stored: the ref is the only
/// state, so there is no map to fall out of step with the sessions it names.
/// (The one exception is a session RESUMED onto an existing conversation, which
/// keeps that conversation's id; `resume` owns that, and this is what every
/// other session gets.)
#[cfg(test)]
fn conversation_id(payload: &Value) -> Result<String, String> {
    conversation_id_for(string_field(payload, "session_id")?)
}

/// A session id is the one ref component we do not author, so it is validated
/// through the protocol's own parser rather than trusted — whether it arrives
/// in a hook payload or as a tool argument.
fn conversation_id_for(session: &str) -> Result<String, String> {
    let id = format!("{SESSION_PREFIX}{session}");
    refs::validate_conversation_id(&id)?;
    Ok(id)
}

fn string_field<'a>(payload: &'a Value, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("hook payload has no string {key}"))
}

/// The assistant's closing message, and the end of the request it answered.
fn on_stop(t: &GitTransport, options: &TurnOptions, payload: &Value) -> Result<(), String> {
    let id = recorded_conversation(t, payload)?;
    let message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let recorded = record_event(t, options, &id, "stop", &[("text", message)])?;
    if recorded.status == "held" {
        eprintln!(
            "caos mcp hook: a call is still running; leaving its request open for the result"
        );
    }
    announce(&format!("caos head {}", recorded.head))
}

/// Show the person running the session where it is recorded: the conversation's
/// ref once, when the first prompt creates it, and its head after every turn.
///
/// A `systemMessage` goes to the USER, not the model. Without it the head is
/// reachable only by asking the model to call `help`, and the id itself
/// (`cc/<Claude Code's internal session id>`) appears in no URL.
fn announce(text: &str) -> Result<(), String> {
    let message = json!({ "systemMessage": text });
    println!(
        "{}",
        serde_json::to_string(&message)
            .map_err(|error| format!("encoding hook response: {error}"))?
    );
    Ok(())
}

/// A turn that ended badly closes its request as failed.
fn on_stop_failure(t: &GitTransport, options: &TurnOptions, payload: &Value) -> Result<(), String> {
    let id = recorded_conversation(t, payload)?;
    let kind = payload
        .get("error_type")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = payload
        .get("error_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let error = match detail.trim().is_empty() {
        true => kind.to_string(),
        false => format!("{kind}: {detail}"),
    };
    record_event(t, options, &id, "failure", &[("text", &error)]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_valid_conversation_ids() {
        let payload = json!({"session_id": "88888abc-9ae5-4d07-a44d-54b366776bdc"});
        assert_eq!(
            conversation_id(&payload).unwrap(),
            "cc/88888abc-9ae5-4d07-a44d-54b366776bdc"
        );
    }

    /// A session id is the one part of a conversation id we do not author.
    /// v3 puts the id in its ref path HEX-ENCODED, so a hostile one cannot
    /// escape the namespace or name a directory -- which is what this pins,
    /// rather than a rejection the protocol deliberately does not perform.
    #[test]
    fn hostile_session_ids_cannot_escape_the_ref_namespace() {
        for hostile in ["../../etc", "a/../b", "with space", "head", "a.lock"] {
            let payload = json!({ "session_id": hostile });
            let id = conversation_id(&payload).unwrap();
            let refname = conversation_ref(&id).unwrap();
            assert!(
                refname.starts_with("refs/caos/v3/conversations/"),
                "session id {hostile:?} named {refname}"
            );
            assert_eq!(refs::parse_head_ref(&refname).unwrap(), id);
        }
    }

    #[test]
    fn a_payload_without_its_event_name_is_an_error() {
        assert!(string_field(&json!({}), "hook_event_name").is_err());
        assert!(string_field(&json!({"hook_event_name": 2}), "hook_event_name").is_err());
    }
}
