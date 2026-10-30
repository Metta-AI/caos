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
//! NOTHING HERE READS A CHECKOUT. A process knows the server it talks to (from
//! `--server=<url>`, or the environment), the step that runs tools
//! (`--llm-step:@@=<locator>`) and, for a conversation's first prompt, where its
//! content comes from (`--base:@@=<locator>`); the rest is a request to the
//! server. A harness that wants uncommitted files in a conversation commits and
//! pushes them first, and names the commit.
//!
//! AND THE TOOLS ARE ORDINARY TOOLS. Nothing here implements one. A call is
//! one request to the server: `llm-step` in its tools-only mode, given the call,
//! declares it as the model's own and runs it exactly as it runs a call in a
//! turn it drives itself, and answers with what the tool said — so `caos mcp`
//! offers whatever that step offers, and a tool reads one way wherever a model
//! meets it.
//!
//! What Claude Code keeps is the MODEL. It chose the call and will read the
//! result; the request a prompt admits is claimed by the step in the same
//! write, because the turn is already running by the time the hook fires.

mod diag;
mod fork;
mod serve;
mod session_base;

use std::io::Read;

use serde_json::{json, Value};

use caos::HttpTransport;
use conversation_protocol::v3::oid::Oid;
use conversation_protocol::v3::refs;

use crate::{conversation_ref, TurnOptions, LLM_STEP_ARG};

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

/// What a `caos mcp` process is told, by its arguments: where the server is,
/// which step runs the tools, and where a new conversation's content comes from.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// `--server=<url>`; the environment otherwise ([`Config::server`]).
    server: Option<String>,
    /// The `--llm-step:@@=<locator>` argument, verbatim.
    step: Option<String>,
    /// `--base:@@=<locator>`: the content a new conversation starts with.
    base: Option<String>,
    /// `--max-result-chars=<n>`: sent to the server with every call, which keeps a
    /// longer tool text in the conversation and answers with a preview. Absent
    /// means the server's default (50000); 0 for no limit.
    max_result_chars: Option<usize>,
}

impl Config {
    fn parse(args: &[String]) -> Result<(Self, Vec<&str>), String> {
        let mut config = Config::default();
        let mut turn = TurnOptions::default();
        let mut rest = Vec::new();
        for argument in args {
            if argument.starts_with("--secret-readers=") {
                // Read by `secret_store::reader_keys`, before any command runs.
            } else if let Some(count) = argument.strip_prefix("--max-result-chars=") {
                config.max_result_chars = Some(count.parse().map_err(|_| {
                    format!("--max-result-chars must be a non-negative integer, not {count:?}")
                })?);
            } else if let Some(url) = argument.strip_prefix("--server=") {
                config.server = Some(url.to_string());
            } else if let Some(locator) = argument.strip_prefix("--base:@@=") {
                config.base = Some(locator.to_string());
            } else if argument.starts_with("--base=") || argument.starts_with("--base:") {
                return Err(format!(
                    "{argument}: a conversation's content is named by locator, \
                     `--base:@@=<git locator>`, and the commit it names is pushed first"
                ));
            } else if !turn.take_image_arg(argument) {
                rest.push(argument.as_str());
            }
        }
        config.step = turn.llm_step;
        Ok((config, rest))
    }

    /// The server this process talks to.
    fn server(&self) -> Result<String, String> {
        self.server
            .clone()
            .ok_or_else(|| "no caos server: pass --server=<url>".to_string())
    }
}

/// `--llm-step:@@=<locator>` names the step that runs this session's tools,
/// exactly as `tui` and `chat` take it. It has to be a locator (or a hash): a
/// path names a file in a checkout, and nothing here has one.
pub fn cli_mcp(args: &[String]) -> Result<(), String> {
    let (config, rest) = Config::parse(args)?;
    match rest.first().copied() {
        Some("hook") => hook(&config),
        Some("diag") => diag::cli(&rest[1..]),
        Some("serve") => serve::serve(config),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    format!(
        "usage:\n  \
         caos mcp hook    (reads one Claude Code hook payload on stdin)\n  \
         caos mcp serve   (workspace tool server; JSON-RPC on stdio)\n\n\
         Both take the server (--server=<url>) and name the step that \
         runs this session's tools:\n  \
         {}\n\
         and the hook takes `--base:@@=<git locator>` for what a new conversation starts from.\n\
         `serve` takes `--max-result-chars=<n>`: a tool result longer than that is kept in \
         the conversation and answered with a preview (default 50000; 0 for no limit).",
        crate::missing_image_arg(LLM_STEP_ARG)
    )
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
/// that is the whole of the client's part: nothing is read from disk, written, or
/// pushed first.
fn step_request(config: &Config, id: &str, params: &[(&str, &str)]) -> Result<StepRequest, String> {
    let server = config.server()?;
    let secrets = caos::Secrets::current().for_conversation(id);
    // REACHABLE FIRST, in front of everything below. Resolving the step talks
    // to the server, and in a cloud session that server is reached through a
    // tunnel whose far end, when gone, accepts and swallows: the wait is
    // unbounded, and this is the path a HOOK takes. A `UserPromptSubmit` hook
    // that never returns takes the session's prompt with it, which presents as
    // Claude Code hanging on the first thing you type, with nothing saying why.
    wait_server_reachable(&server)?;
    let step = step_image(config, &server, &secrets)?;
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
        server,
        params: all,
        secrets,
    })
}

/// Run the step on a conversation and wait: the result's `(kind, hash)`.
fn run_step(
    config: &Config,
    id: &str,
    params: &[(&str, &str)],
) -> Result<(String, String), String> {
    let request = step_request(config, id, params)?;
    let all = request.params();
    caos::eval_line_request(&request.server, "run", &all, &request.secrets)
}

/// The step, resolved by the server from its locator.
///
/// A path (`--llm-step:@=std/llm-step`) names a file in a checkout, and a
/// process with no checkout cannot answer it: it is refused by name rather than
/// resolved against whatever directory the harness happened to start in.
fn step_image(config: &Config, server: &str, secrets: &caos::Secrets) -> Result<String, String> {
    if config
        .step
        .as_deref()
        .is_some_and(|argument| argument.starts_with(&format!("--{LLM_STEP_ARG}:@=")))
    {
        return Err(format!(
            "--{LLM_STEP_ARG}:@=<path> names a file in a checkout, and `caos mcp` reads none; \
             name the step by locator: --{LLM_STEP_ARG}:@@=git+https://…?rev=<sha>&dir=std/{LLM_STEP_ARG}"
        ));
    }
    crate::resolve_image_arg(
        &HttpTransport::new(server),
        config.step.as_deref(),
        LLM_STEP_ARG,
        secrets,
        None,
    )
}

/// Wait, bounded, for the caos server to answer -- the hook's counterpart to
/// the serve resolver's retry loop.
///
/// The budget is a compromise between the two ways this hurts. Too short and
/// the first prompt races tunnel bringup and loses (the bug this exists for);
/// too long and a genuinely-down server hangs the prompt, since a
/// `UserPromptSubmit` hook blocks the turn until it returns. Each attempt is
/// `check_server_reachable`'s own 5s round trip, so `HOOK_REACH_ATTEMPTS`
/// probes plus the interstitial sleeps is the ceiling -- ~35s, which outlasts
/// tunnel bringup. The common case returns on the first probe.
///
/// THE HOOK'S OWN TIMEOUT IS SET, in `shared/settings.json`, because Claude
/// Code's 60s default is not enough over iroh: this wait plus the first
/// prompt's content fetch exceeded it in a cloud session, and a killed hook
/// lets the prompt through with no conversation and no reason given.
const HOOK_REACH_ATTEMPTS: u32 = 6;
const HOOK_REACH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) fn wait_server_reachable(server: &str) -> Result<(), String> {
    let mut last = caos::check_server_reachable(server);
    let mut attempt = 1;
    while last.is_err() && attempt < HOOK_REACH_ATTEMPTS {
        std::thread::sleep(HOOK_REACH_INTERVAL);
        last = caos::check_server_reachable(server);
        attempt += 1;
    }
    last
}

/// Dispatch one hook payload. An event we do not record is not an error: Claude
/// Code fires many, and a settings file that routes extra ones here should keep
/// working rather than failing a turn.
///
/// A prompt with no server to record into is refused outright (exit 2): the
/// diagnostics go first (stderr and the diag journal), because exit 2 is the
/// one status with which Claude Code shows the reason, and a session that runs
/// on with nothing recorded is worse than one that stops. Elsewhere a failed
/// hook is shown only as an error line, and exit 2 on `Stop` would force the
/// session on.
fn hook(config: &Config) -> Result<(), String> {
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
    if event == "UserPromptSubmit" {
        if let Err(error) = config.server() {
            diag::emit(&diag::Context {
                event,
                source: payload.get("source").and_then(Value::as_str),
                session: Some(session),
                reason: Some(&error),
            });
            eprintln!("caos: this session has no server, so caos did nothing: {error}");
            std::process::exit(2);
        }
    }
    let result = match event {
        "UserPromptSubmit" => on_user_prompt(config, &payload),
        "PreToolUse" => on_pre_tool_use(&payload),
        "Stop" => on_stop(config, &payload),
        _ => on_stop_failure(config, &payload),
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

/// The user's prompt, and the only event allowed to create the conversation:
/// the first prompt of a session establishes its base and fallback title
/// exactly as the TUI's first message does.
fn on_user_prompt(config: &Config, payload: &Value) -> Result<(), String> {
    let prompt = string_field(payload, "prompt")?;
    if prompt.trim().is_empty() {
        return Ok(());
    }
    // `/fork-caos-conversation <hash>` makes the conversation this session records
    // into, as a fork of that commit. The step does it with the first prompt, and
    // does nothing once the session has its conversation.
    //
    // A refusal exits 2, the one status with which Claude Code blocks the prompt
    // and shows the reason: a session that ran on would have no conversation
    // behind it, and a tool call would fail with a reason nobody sees.
    let from = match fork::parse_command(prompt) {
        Ok(from) => from,
        Err(error) => refuse_prompt(&error),
    };
    let id = recorded_conversation(payload)?;
    let readers = crate::secret_store::reader_summary();
    let status = match record_prompt(config, &id, prompt, from.as_ref()) {
        Ok(status) => status,
        Err(error) if from.is_some() => refuse_prompt(&error),
        Err(error) => return Err(error),
    };
    match (status.as_str(), &from) {
        ("forked", Some(from)) => announce(&format!(
            "caos conversation ref {} forked from {from}; {readers}",
            conversation_ref(&id)?
        )),
        ("created", _) => announce(&format!(
            "caos conversation ref {}; {readers}",
            conversation_ref(&id)?
        )),
        _ => Ok(()),
    }
}

/// What the step answered to a recorded event: which of its outcomes it was
/// (`created`, `forked`, `recorded`, `nothing`) and the head it left.
struct Recorded {
    status: String,
    head: String,
}

/// Send one event to the step (`llm-step --record=<kind>`) and read its answer.
///
/// A hook is one request: the step makes the transitions on the server, so this
/// process needs no checkout of the conversation, writes nothing, and a repeated
/// event (a retried hook) names the message id it already recorded and writes
/// nothing twice.
fn record_event(
    config: &Config,
    id: &str,
    kind: &str,
    extra: &[(&str, &str)],
) -> Result<Recorded, String> {
    let message_id = caos::fresh_entropy()?;
    let username = username()?;
    let mut params = vec![
        ("record", kind),
        ("message-id", message_id.as_str()),
        ("actor", username.as_str()),
    ];
    params.extend_from_slice(extra);
    let (answer, tree) = run_step(config, id, &params)?;
    if answer != "tree" {
        return Err(format!("the step answered a {answer}, not a record"));
    }
    let files = caos::tree_blobs(&HttpTransport::new(&config.server()?), &tree)?;
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

/// Who is acting: `$USER`, as every caos client says it.
fn username() -> Result<String, String> {
    Ok(crate::ambient_username(std::env::var("USER"))?.unwrap_or_else(|| "user".to_string()))
}

/// Record a user's prompt, creating the conversation on the session's first one;
/// the step's status says what happened (`created`, `forked`, `recorded`, ...).
///
/// The step appends the message and nothing else: a conversation keeps no turn
/// state, so there is no request to open before Claude Code's calls are
/// recorded.
///
/// A NEW conversation needs where it starts from, which the step cannot find for
/// itself: `from`, the commit it forks, and the locator naming its content. With
/// `from` alone the fork keeps its source's content; with both, the content
/// commit is merged into the fork, which is how a session picks a conversation
/// up on a newer client repo. Both go with every prompt and the step uses them
/// only when the conversation does not exist yet.
fn record_prompt(
    config: &Config,
    id: &str,
    prompt: &str,
    from: Option<&Oid>,
) -> Result<String, String> {
    let mut extra = vec![("text", prompt), ("model", CLAUDE_CODE_MODEL)];
    let from = from.map(|from| from.to_string());
    let content = session_base::content_locator(config.base.as_deref(), prompt);
    // Both go: a root is seeded from the content, a fork merges it into the
    // forked conversation (`std/llm-step/src/record.rs`). The step imports the
    // commit the locator names, so the hook resolves nothing.
    if let Some(from) = &from {
        extra.push(("from", from));
    }
    if let Some(content) = &content {
        extra.push(("content-locator", content));
    }
    let phase = std::time::Instant::now();
    let recorded = record_event(config, id, "prompt", &extra)?;
    cc_timing("record", phase.elapsed());
    Ok(recorded.status)
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

/// The conversation a hook payload's session records into.
fn recorded_conversation(payload: &Value) -> Result<String, String> {
    conversation_id_for(string_field(payload, "session_id")?)
}

/// A session's conversation id is derived, never stored: the ref is the only
/// state, so there is no map to fall out of step with the sessions it names.
///
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

/// The assistant's closing message. A call still running in the background is
/// unaffected: its result is recorded whenever it arrives.
fn on_stop(config: &Config, payload: &Value) -> Result<(), String> {
    let id = recorded_conversation(payload)?;
    let message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let recorded = record_event(
        config,
        &id,
        "stop",
        &[("text", message), ("model", CLAUDE_CODE_MODEL)],
    )?;
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

fn on_stop_failure(config: &Config, payload: &Value) -> Result<(), String> {
    let id = recorded_conversation(payload)?;
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
    record_event(config, &id, "failure", &[("text", &error)]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limit is an argument, not an environment variable, and it is the
    /// server that applies it; this process only forwards it.
    #[test]
    fn the_limit_is_an_argument_and_zero_means_none() {
        let parse = |list: &[&str]| -> Result<Config, String> {
            let given: Vec<String> = list.iter().map(|s| s.to_string()).collect();
            Config::parse(&given).map(|(config, _)| config)
        };
        assert_eq!(parse(&["serve"]).unwrap().max_result_chars, None);
        assert_eq!(
            parse(&["serve", "--max-result-chars=0"])
                .unwrap()
                .max_result_chars,
            Some(0)
        );
        assert_eq!(
            parse(&["serve", "--max-result-chars=800"])
                .unwrap()
                .max_result_chars,
            Some(800)
        );
        assert!(parse(&["serve", "--max-result-chars=lots"]).is_err());
    }

    #[test]
    fn session_ids_become_valid_conversation_ids() {
        let payload = json!({"session_id": "88888abc-9ae5-4d07-a44d-54b366776bdc"});
        assert_eq!(
            recorded_conversation(&payload).unwrap(),
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
            let id = recorded_conversation(&payload).unwrap();
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

#[cfg(test)]
mod config_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_arguments_name_the_server_the_step_and_the_base() {
        let given = args(&[
            "hook",
            "--server=http://s",
            "--llm-step:@@=github:o/r?rev=abc",
            "--base:@@=github:o/c?rev=def",
        ]);
        let (config, rest) = Config::parse(&given).unwrap();
        assert_eq!(rest, ["hook"]);
        assert_eq!(config.server.as_deref(), Some("http://s"));
        assert_eq!(
            config.step.as_deref(),
            Some("--llm-step:@@=github:o/r?rev=abc")
        );
        assert_eq!(config.base.as_deref(), Some("github:o/c?rev=def"));
    }

    /// `--base=<sha>` named a commit in a checkout, and nothing reads one now.
    #[test]
    fn a_base_that_is_not_a_locator_is_refused_with_the_replacement() {
        for old in [
            "--base=0123456789abcdef0123456789abcdef01234567",
            "--base:@=std",
        ] {
            let error = Config::parse(&args(&["hook", old])).unwrap_err();
            assert!(error.contains("--base:@@=<git locator>"), "{error}");
        }
    }
    #[test]
    fn a_server_is_named_by_argument_only() {
        let named = Config {
            server: Some("http://arg".into()),
            ..Config::default()
        };
        assert_eq!(named.server().unwrap(), "http://arg");
        let error = Config::default().server().unwrap_err();
        assert!(error.contains("--server=<url>"), "{error}");
    }
}
