//! `POST /mcp`: the caos tool server, as MCP over HTTP.
//!
//! One JSON-RPC message per request, answered in the body (MCP's streamable
//! HTTP transport, without the optional SSE stream: nothing here is pushed to a
//! client). A harness that can reach this server by URL names it in its MCP
//! config; one that can only reach it through `caos://` runs `caos-cli mcp
//! serve`, which carries these same bytes over that transport and implements
//! none of the protocol.
//!
//! THE TOOLS ARE FIVE, AND THEY NEVER CHANGE: `run`, `help`, `start`, `result`
//! and `eval-path`. They are the server's own primitives, and what they run is
//! named by a PATH in the conversation (`caos-std/read`), so there is nothing to
//! list and nothing to resolve first: `tools/list` is a constant, and a client
//! that reads it once or ignores `tools/list_changed` is correct, not stale.
//!
//! A call is `llm-step` in its tools-only mode, evaluated by the server the way
//! `POST /eval/run` evaluates anything: the step declares the call (an assistant
//! message carrying its `tool_use`), runs it, and answers with the call's record.
//! Nothing here writes a conversation.
//!
//! WHAT A REQUEST CARRIES, as headers, because a request is all a stateless
//! server has: `X-Caos-Step` names the step that runs the tools (a git locator,
//! or a bare 40-hex hash), and `X-Caos-Secret-Readers` the reader keys, as
//! everywhere else. The conversation is not a header: each tool call names its
//! session in the `caos_session` argument, which the harness's PreToolUse hook
//! fills in, and the conversation id derives from that.

use std::sync::Arc;

use serde_json::{json, Value};

use conversation_protocol::v3::refs;

use caos_world::mcp::{fail, initialize, reply, tools, INSTRUCTIONS, SESSION_ARG};

use crate::{compute, storage, Config};

/// Conversation ids for recorded sessions live under one component so they are
/// obvious in the sidebar and cannot collide with a hand-named conversation.
const SESSION_PREFIX: &str = "cc/";

pub(crate) struct Caller {
    /// `X-Caos-Step`: a git locator, or a 40-hex hash.
    pub(crate) step: Option<String>,
    /// `X-Caos-Secret-Readers`, as sent.
    pub(crate) readers: String,
    /// `X-Caos-Max-Result-Chars`: how long a tool's text may be before it is kept
    /// in the conversation and answered with a preview ([`DEFAULT_MAX_RESULT_CHARS`]
    /// when the header is absent; 0 for no limit).
    pub(crate) max_result_chars: usize,
}

/// Answer one JSON-RPC message. `None` is a notification (no `id`): JSON-RPC
/// forbids answering one, and the HTTP layer says `202 Accepted` with no body.
pub(crate) fn handle(config: &Arc<Config>, caller: &Caller, body: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(error) => {
            return Some(fail(Value::Null, -32700, &format!("parse error: {error}")));
        }
    };
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => reply(id, initialize(&params)),
        "tools/list" => reply(id, json!({ "tools": tools() })),
        "tools/call" => match call(config, caller, &params) {
            Ok(result) => reply(id, result),
            // A tool that could not run at all is a JSON-RPC error; one that ran
            // and failed is a result with `isError`, which the model sees.
            Err(error) => fail(id, -32603, &error),
        },
        "ping" => reply(id, json!({})),
        other => fail(id, -32601, &format!("unknown method {other:?}")),
    })
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// The step's name for each of this server's tools. The step implements them
/// under the names it has always had; the model reads the others.
fn step_tool(name: &str) -> Option<&'static str> {
    match name {
        "run" | "start" => Some("run_tool"),
        "help" => Some("tool_help"),
        "eval-path" => Some("eval_path"),
        _ => None,
    }
}

/// A session's conversation id is derived, never stored: the ref is the only
/// state. A session id is the one ref component we do not author, so it is
/// validated through the protocol's own parser rather than trusted.
fn conversation_id_for(session: &str) -> Result<String, String> {
    let id = format!("{SESSION_PREFIX}{session}");
    refs::validate_conversation_id(&id)?;
    Ok(id)
}

/// The model's own arguments, without the values the hook injected: those are
/// plumbing, and showing them in the transcript would misrepresent the call.
fn declared_args(args: &Value) -> Value {
    let mut declared = args.clone();
    if let Some(object) = declared.as_object_mut() {
        object.remove("caos_session");
        object.remove("caos_tool_use_id");
        object.remove("caos_prompt_id");
    }
    declared
}

fn call(config: &Arc<Config>, caller: &Caller, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call has no tool name".to_string())?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let session = args.get(SESSION_ARG).and_then(Value::as_str);
    // `help` with no path reports on the server and belongs to no conversation
    // call: it takes the session only to say which conversation that is.
    if name == "help"
        && args
            .get("path")
            .and_then(Value::as_str)
            .is_none_or(|path| path.trim().is_empty())
    {
        return Ok(overview(config, caller, session));
    }
    let session = session.ok_or_else(|| {
        format!(
            "{SESSION_ARG} is missing: the PreToolUse hook that supplies it \
             is not installed, so this call cannot be attributed to a conversation"
        )
    })?;
    if name == "result" {
        let ticket = args
            .get("ticket")
            .and_then(Value::as_str)
            .ok_or_else(|| "result needs the `ticket` start returned".to_string())?;
        return tool_result(config, ticket.trim(), caller.max_result_chars);
    }
    let step_name = step_tool(name).ok_or_else(|| format!("unknown tool {name:?}"))?;
    let conversation = conversation_id_for(session)?;
    let secrets = crate::secrets::Context::admit(config, &caller.readers, Some(&conversation))
        .map_err(|e| e.message().to_string())?;
    let params = step_params(caller, &conversation, step_name, &args)?;
    if name == "start" {
        // The ticket goes back at once, and the job declares its call and runs it
        // whenever it gets there. The model may end its turn first: a turn's end
        // is one more fact in the transcript, and closes nothing the call needs.
        let ticket = compute::start_expression(config, "run", &params, &secrets)
            .map_err(|e| e.message().to_string())?;
        let ticket = String::from_utf8_lossy(&ticket).trim().to_string();
        return Ok(text_result(
            format!(
                "ticket: {ticket}\n\nAsk for it with result(ticket=\"{ticket}\"). It is \
                 pending until the tool finishes."
            ),
            false,
        ));
    }
    let answer = compute::eval_expression_endpoint(config, "run", &params, &secrets)
        .map_err(|e| e.message().to_string())?;
    let answer = String::from_utf8_lossy(&answer);
    let (kind, hash) = answer
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("the step answered {:?}", answer.trim()))?;
    decode_result(config, kind, hash, caller.max_result_chars)
}

/// The form a tools-only run of the step takes, as the `/eval/run` parameter
/// string: the step, the conversation, and the call.
fn step_params(
    caller: &Caller,
    conversation: &str,
    step_name: &str,
    args: &Value,
) -> Result<String, String> {
    let step = caller.step.as_deref().ok_or_else(|| {
        "no step: this request carried no X-Caos-Step header naming the step that runs tools"
            .to_string()
    })?;
    let call = args
        .get("caos_tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(step_name);
    let step_arg = if step.len() == 40 && step.bytes().all(|b| b.is_ascii_hexdigit()) {
        ("base:hash", step)
    } else {
        ("base:@@", step)
    };
    let arguments = declared_args(args).to_string();
    // The step saves what is longer than this in the conversation and answers
    // with where (see `spill_notice`).
    let limit = caller.max_result_chars.to_string();
    let pairs = [
        step_arg,
        ("conversation", conversation),
        ("tools-only", call),
        ("tool", step_name),
        ("arguments", arguments.as_str()),
        ("max-result-chars", limit.as_str()),
    ];
    Ok(pairs
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&"))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The state of a started tool, from its ticket.
fn tool_result(config: &Arc<Config>, ticket: &str, limit: usize) -> Result<Value, String> {
    if ticket.len() != 40 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{ticket:?} is not a ticket; `start` returns one"));
    }
    let state = compute::expression_result(config, ticket).map_err(|e| e.message().to_string())?;
    let state = String::from_utf8_lossy(&state).trim().to_string();
    if state == "pending" {
        return Ok(text_result(
            "pending: the call has not finished. Ask for `result` again with this ticket."
                .to_string(),
            false,
        ));
    }
    if let Some(rest) = state.strip_prefix("ok ") {
        let (kind, hash) = rest
            .split_once(' ')
            .ok_or_else(|| format!("malformed result {state:?}"))?;
        return decode_result(config, kind, hash, limit);
    }
    if let Some(rest) = state.strip_prefix("failed ") {
        let (status, message) = rest.split_once(' ').unwrap_or((rest, ""));
        return Ok(text_result(
            format!("the call failed ({status}): {message}"),
            true,
        ));
    }
    Err(format!("unrecognised ticket state {state:?}"))
}

/// What a tools-only run answered, as the model reads it: the call's `status`
/// and `observation`, the same bytes the conversation recorded.
fn decode_result(config: &Config, kind: &str, hash: &str, limit: usize) -> Result<Value, String> {
    if kind != "tree" {
        return Err(format!("the step answered a {kind}, not a tool result"));
    }
    let entries = storage::fetch_tree(config, hash)?;
    let part = |file: &str| -> Result<Vec<u8>, String> {
        let entry = entries
            .iter()
            .find(|entry| entry.name == file)
            .ok_or_else(|| format!("the step's result {hash} has no {file}"))?;
        storage::fetch_blob(config, &entry.oid.to_string())
    };
    let status = String::from_utf8_lossy(&part("status")?).into_owned();
    let (is_error, text) = tool_outcome(status.trim(), &part("observation")?)?;
    let text = match entries.iter().any(|entry| entry.name == "spilled") {
        true => spill_notice(&text, limit, &String::from_utf8_lossy(&part("spilled")?)),
        false => text,
    };
    Ok(text_result(text, is_error))
}

/// A tool's text longer than this many characters is kept in the conversation
/// and answered with a preview. Claude Code refuses an MCP result past 25,000
/// tokens, and what it does instead is write the whole thing to a file on ITS
/// disk and tell the model to read that -- a path the model usually cannot open
/// (a cloud session has no file tools to open it with). The limit sits well
/// under that ceiling, because characters per token vary: dense output such as
/// JSON or a trace tokenizes far worse than prose.
pub(crate) const DEFAULT_MAX_RESULT_CHARS: usize = 50_000;

/// How much of a spilled result the answer still carries, so the model can tell
/// what it is before deciding how to page through it.
const RESULT_PREVIEW_CHARS: usize = 2_000;

/// What the model reads instead of a result the step saved in the conversation:
/// the start of it, cut at a line, and how to get the rest. `spilled` is the
/// step's own description of where it went: the path, then the blob's hash.
///
/// `read` takes either: `file-path` for the path, or the hash as a bare `root`
/// with no `file-path`.
fn spill_notice(text: &str, limit: usize, spilled: &str) -> String {
    let (path, blob) = spilled.split_once('\n').unwrap_or((spilled, ""));
    let (path, blob) = (path.trim(), blob.trim());
    let keep = RESULT_PREVIEW_CHARS.min(limit);
    let end = text
        .char_indices()
        .nth(keep)
        .map_or(text.len(), |(index, _)| index);
    let mut preview = &text[..end];
    if let Some(newline) = preview.rfind('\n').filter(|&n| n > 0) {
        preview = &preview[..newline];
    }
    let chars = text.chars().count();
    let lines = text.lines().count();
    let by_hash = match blob.is_empty() {
        true => String::new(),
        false => format!(", or `read` with root={blob} and no file-path"),
    };
    format!(
        "{preview}\n\n[caos: this result is {chars} characters over {lines} lines, too long to \
         return. The whole of it is saved in the conversation at {path}. Page through it with \
         `read` (file-path={path}, offset, limit){by_hash}.]"
    )
}

/// `(is_error, text)` for a call's recorded output, given which of `complete`,
/// `failed` and `cancelled` it is.
fn tool_outcome(status: &str, observation: &[u8]) -> Result<(bool, String), String> {
    match status {
        "complete" => {
            let value: Value = serde_json::from_slice(observation)
                .map_err(|error| format!("parsing tool observation: {error}"))?;
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let content = value
                .get("content")
                .map(tool_result_text)
                .unwrap_or_else(|| tool_result_text(&value));
            Ok((is_error, content))
        }
        "failed" => {
            let value: Value = serde_json::from_slice(observation).unwrap_or_else(|_| {
                Value::String(String::from_utf8_lossy(observation).into_owned())
            });
            Ok((true, tool_result_text(&value)))
        }
        "cancelled" => Ok((true, String::from_utf8_lossy(observation).into_owned())),
        other => Err(format!("a tool result has no status {other:?}")),
    }
}

fn tool_result_text(value: &Value) -> String {
    if let Some(blocks) = value.as_array() {
        let text = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if !text.is_empty() {
            return text.join("\n");
        }
    }
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
}

/// `help` with no path: how caos is doing here. A model that finds caos missing
/// should be able to ask and get the reason, not conclude caos is absent.
fn overview(config: &Config, caller: &Caller, session: Option<&str>) -> Value {
    let conversation = match session {
        None => format!(
            "conversation: unknown -- this call carried no {SESSION_ARG}, so the \
             PreToolUse hook that supplies it is not installed\n"
        ),
        Some(session) => match conversation_id_for(session) {
            Err(error) => format!("conversation: session {session:?} is unusable: {error}\n"),
            Ok(id) => format!(
                "conversation: {id}\nconversation head: {}\n",
                head_of(config, &id)
            ),
        },
    };
    let step = match caller.step {
        Some(_) => "step: named by this connection\n",
        None => "step: NONE -- this connection carried no X-Caos-Step, so no tool can run\n",
    };
    text_result(
        format!("{INSTRUCTIONS}\n\n{conversation}{step}server: reachable\n"),
        false,
    )
}

fn head_of(config: &Config, conversation: &str) -> String {
    let repo = config.repo.to_thread_local();
    let Ok(refname) = refs::head_ref(conversation) else {
        return "<invalid conversation id>".to_string();
    };
    match repo.find_reference(refname.as_str()) {
        Ok(mut reference) => match reference.peel_to_commit() {
            Ok(commit) => commit.id().to_string(),
            Err(error) => format!("<unreadable: {error}>"),
        },
        Err(_) => "none yet -- no turn has been recorded".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caos_world::mcp::PROTOCOL_VERSION;

    /// The handshake and the list touch no server state, so they can be
    /// checked without a `Config`.
    #[test]
    fn initialize_echoes_a_version_it_can_speak() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2024-11-05"}))["protocolVersion"],
            "2024-11-05"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    /// The model learns how caos fits together from `instructions`.
    #[test]
    fn the_handshake_tells_the_model_how_to_use_the_tools() {
        let instructions = initialize(&json!({}))["instructions"]
            .as_str()
            .unwrap()
            .to_string();
        for needle in [
            "help(path)",
            "run(path",
            "start(",
            "result(",
            "eval-path",
            "caos-std/",
        ] {
            assert!(
                instructions.contains(needle),
                "instructions lack {needle:?}"
            );
        }
    }

    /// The list is a constant and holds exactly these five, in this order.
    #[test]
    fn the_tool_list_is_five_fixed_tools() {
        let names: Vec<String> = tools()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["run", "help", "start", "result", "eval-path"]);
    }

    /// Every tool must accept the injected session arg, or the hook's
    /// `updatedInput` would produce a call that fails schema validation.
    #[test]
    fn every_tool_carries_the_injected_args() {
        for tool in tools() {
            let properties = &tool["inputSchema"]["properties"];
            for injected in [SESSION_ARG, "caos_tool_use_id", "caos_prompt_id"] {
                assert!(
                    properties.get(injected).is_some(),
                    "{} does not declare {injected}",
                    tool["name"]
                );
            }
        }
    }

    #[test]
    fn the_tools_are_the_steps_under_the_names_the_model_reads() {
        assert_eq!(step_tool("run"), Some("run_tool"));
        assert_eq!(step_tool("start"), Some("run_tool"));
        assert_eq!(step_tool("help"), Some("tool_help"));
        assert_eq!(step_tool("eval-path"), Some("eval_path"));
        assert_eq!(step_tool("result"), None);
        assert_eq!(step_tool("read"), None);
    }

    /// The call's parameters name the step as a hash or as a locator, and carry
    /// the model's arguments without the hook's plumbing.
    #[test]
    fn a_call_names_its_step_and_hides_the_injected_args() {
        let args = json!({"path": "caos-std/read", "caos_session": "s", "caos_tool_use_id": "t1"});
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let by_hash = step_params(
            &Caller {
                step: Some(hash.into()),
                readers: String::new(),
                max_result_chars: DEFAULT_MAX_RESULT_CHARS,
            },
            "cc/s",
            "run_tool",
            &args,
        )
        .unwrap();
        assert!(
            by_hash.starts_with(&format!("base:hash={hash}&")),
            "{by_hash}"
        );
        assert!(by_hash.contains("tools-only=t1"), "{by_hash}");
        assert!(!by_hash.contains("caos_session"), "{by_hash}");
        let by_locator = step_params(
            &Caller {
                step: Some("git+caos://local?rev=x&dir=std/llm-step".into()),
                readers: String::new(),
                max_result_chars: DEFAULT_MAX_RESULT_CHARS,
            },
            "cc/s",
            "run_tool",
            &args,
        )
        .unwrap();
        assert!(
            by_locator.starts_with("base:@@=git%2Bcaos%3A%2F%2Flocal"),
            "{by_locator}"
        );
    }

    #[test]
    fn the_notice_keeps_whole_lines_and_names_where_the_rest_is() {
        let text = "alpha\nbeta\ngamma\ndelta\n".repeat(1_000);
        let blob = "ab".repeat(20);
        let spilled = format!("tool-results/toolu_1.txt\n{blob}");
        let notice = spill_notice(&text, DEFAULT_MAX_RESULT_CHARS, &spilled);
        let (preview, rest) = notice.split_once("\n\n[caos:").unwrap();
        assert!(preview.chars().count() <= RESULT_PREVIEW_CHARS);
        assert!(text.starts_with(preview));
        assert!(text[preview.len()..].starts_with('\n'), "cut mid-line");
        assert!(rest.contains("tool-results/toolu_1.txt"));
        assert!(rest.contains(&format!("root={blob}")));
        assert!(rest.contains("4000 lines"));
    }

    #[test]
    fn the_preview_never_splits_a_character_or_outgrows_the_limit() {
        let text = "é".repeat(500);
        let notice = spill_notice(&text, 100, "tool-results/x.txt\n");
        let (preview, rest) = notice.split_once("\n\n[caos:").unwrap();
        assert_eq!(preview.chars().count(), 100);
        assert!(!rest.contains("root="), "no hash was given");
    }

    /// The limit rides to the step with every call, so what it saves and what the
    /// notice says it kept agree.
    #[test]
    fn a_call_carries_the_result_limit_to_the_step() {
        let caller = Caller {
            step: Some("0123456789abcdef0123456789abcdef01234567".into()),
            readers: String::new(),
            max_result_chars: 123,
        };
        let params = step_params(&caller, "cc/s", "run_tool", &json!({})).unwrap();
        assert!(params.contains("max-result-chars=123"), "{params}");
    }
    #[test]
    fn a_call_with_no_step_says_so() {
        let error = step_params(
            &Caller {
                step: None,
                readers: String::new(),
                max_result_chars: DEFAULT_MAX_RESULT_CHARS,
            },
            "cc/s",
            "run_tool",
            &json!({}),
        )
        .unwrap_err();
        assert!(error.contains("X-Caos-Step"), "{error}");
    }

    #[test]
    fn a_tool_outcome_reads_the_recorded_observation() {
        let observation = br#"{"content":[{"type":"text","text":"hi"}],"is_error":false}"#;
        assert_eq!(
            tool_outcome("complete", observation).unwrap(),
            (false, "hi".to_string())
        );
        assert_eq!(
            tool_outcome("cancelled", b"stopped").unwrap(),
            (true, "stopped".to_string())
        );
        assert!(tool_outcome("weird", b"").is_err());
    }
}
