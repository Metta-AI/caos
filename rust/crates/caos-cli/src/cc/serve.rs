//! The tool server Claude Code spawns: newline-delimited JSON-RPC on stdio.
//!
//! This is an ordinary command-line program. Claude Code runs it as a child
//! process and exchanges one JSON object per line with it; the protocol is
//! small enough (initialize, tools/list, tools/call) that implementing it
//! directly costs less than a dependency would. That matters here specifically:
//! `std/cargo` builds `--offline` against a vendored registry, so every new
//! crate has to be re-anchored in the bake (`tests/lint/lint-bake-anchor.sh`)
//! before anything can compile.
//!
//! STDOUT CARRIES THE PROTOCOL. Nothing may print to it but a response —
//! a stray `println!` desynchronizes the stream and the session dies with a
//! parse error naming neither this file nor the line that wrote. Diagnostics go
//! to stderr, which Claude Code surfaces without reading it as a message.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use caos::GitTransport;

use crate::TurnOptions;

/// The version we implement. A client that asks for another gets its own value
/// echoed back when we can speak it, per MCP's negotiation rule.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];

/// The arg the `PreToolUse` hook injects. It is declared in every schema rather
/// than smuggled in, so the model's own call is valid with or without it and
/// the hook is only supplying a value the tool always accepted.
const SESSION_ARG: &str = "caos_session";

/// How long the tool resolution keeps trying: 20 attempts, 15 seconds apart,
/// so five minutes of a session's setup being slow or out of order costs
/// nothing. A cold BUILD is not what this waits out -- that happens inside one
/// attempt -- it is a `caos` remote or a tunnel that does not exist yet.
const RESOLVE_ATTEMPTS: u32 = 20;
const RESOLVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The workspace is passed in UNRESOLVED, and a failure to open it does not
/// stop the server.
///
/// Claude Code reports a tool server that dies before it speaks as
/// `CONNECTION_CLOSED`, which says nothing about a repository, a directory or
/// caos, and whatever it printed goes wherever a dead child's stderr goes.
/// Answering `initialize` and then naming the problem on the first tool call
/// puts the reason in front of the person who can fix it.
pub fn serve(workspace: Result<GitTransport, String>, options: TurnOptions) -> Result<(), String> {
    let workspace = match workspace {
        Ok(t) => Ok(t),
        Err(error) => {
            eprintln!("caos cc serve: cannot open the caos workspace: {error}");
            eprintln!("caos cc serve: serving anyway; tools will report this when called");
            Err(error)
        }
    };
    let have_workspace = workspace.is_ok();
    let t = workspace.as_ref();
    let registry: Registry = Arc::new(Mutex::new(Vec::new()));
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("reading request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let method = method_of(&line);
        let Some(response) = handle(t, &options, &registry, &line) else {
            continue;
        };
        write_message(&out, &response)?;
        // The handshake is answered; NOW go and find out what the tools are.
        // Started here rather than before the loop so the notification it ends
        // with cannot precede `initialize`, and only once.
        if method.as_deref() == Some("initialize") && have_workspace {
            resolve_in_background(options.clone(), Arc::clone(&registry), Arc::clone(&out));
        }
    }
    Ok(())
}

/// The tools this server has managed to find so far. Empty until the
/// resolution below finishes, which is the entire point: see `declarations`.
type Registry = Arc<Mutex<Vec<Value>>>;

/// Stdout, shared with the resolver thread. Its own lock, not stdout's: the
/// protocol is one message per line, and two writers interleaving mid-message
/// would corrupt the stream that `std::io::Stdout`'s internal lock protects
/// only per write call.
type Out = Arc<Mutex<std::io::Stdout>>;

fn write_message(out: &Out, message: &Value) -> Result<(), String> {
    let encoded =
        serde_json::to_string(message).map_err(|error| format!("encoding response: {error}"))?;
    let mut out = out
        .lock()
        .map_err(|_| "the output lock is poisoned".to_string())?;
    writeln!(out, "{encoded}").map_err(|error| format!("writing response: {error}"))?;
    out.flush()
        .map_err(|error| format!("flushing response: {error}"))
}

/// Find the tools, off the hot path, and say so when they arrive.
///
/// THIS IS WHY THE SERVER CONNECTS AT ALL. Asking the step what it offers
/// means fetching it (a pinned locator is another repo), evaluating it against
/// the caos server, and — the first time in a tree — BUILDING it, which is a
/// rustc compile measured in minutes. Done inside `tools/list`, that outlasts
/// the client's startup budget by two orders of magnitude, and the session sees
/// a tool server that never answered rather than one still working.
///
/// So `tools/list` answers immediately with whatever is known (nothing, at
/// first) and this thread does the work. `notifications/tools/list_changed` is
/// the protocol's own answer to exactly this: the client re-lists when it
/// arrives, and the tools appear when they are ready.
fn resolve_in_background(options: TurnOptions, registry: Registry, out: Out) {
    std::thread::spawn(move || {
        // RETRIED, because this races the session's own setup. The client
        // reaches caos through a `caos` git remote and a tunnel, and in a cloud
        // container BOTH are established by the SessionStart hook -- which runs
        // when the session starts, not before this server is spawned. A single
        // attempt that lost that race would find no server, give up, and leave
        // the session with an empty tool list and no second chance.
        //
        // Only "not ready yet" is worth retrying, and there is no way to tell
        // that from any other failure, so everything is: the cost of a wrong
        // guess is a few sleeping seconds in a thread nothing waits on.
        let mut found = Vec::new();
        for attempt in 0..RESOLVE_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(RESOLVE_INTERVAL);
            }
            // Its OWN transport, opened per attempt: the one in `serve` belongs
            // to the main thread, and a transport opened before the remote
            // existed would not have it. This process already stands in the
            // work directory (`cc_transport`).
            found = match GitTransport::from_cwd() {
                Ok(t) => declarations(Ok(&t), &options),
                Err(error) => {
                    eprintln!("caos cc serve: cannot open the caos workspace: {error}");
                    Vec::new()
                }
            };
            if !found.is_empty() {
                break;
            }
        }
        if found.is_empty() {
            eprintln!(
                "caos cc serve: no tools after {RESOLVE_ATTEMPTS} attempts; \
                 this session has none. The reasons are above."
            );
            return;
        }
        match registry.lock() {
            Ok(mut registry) => *registry = found,
            Err(_) => {
                eprintln!("caos cc serve: the tool registry lock is poisoned");
                return;
            }
        }
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
        });
        if let Err(error) = write_message(&out, &notification) {
            eprintln!("caos cc serve: could not announce the tools: {error}");
        }
    });
}

/// The `method` of one request line, for a caller that has already had it
/// handled and needs to know what it was.
fn method_of(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("method")?
        .as_str()
        .map(str::to_string)
}

/// Handle one message. `None` means "say nothing", which is required rather
/// than merely polite: a JSON-RPC notification has no `id`, and answering one
/// is a protocol violation.
fn handle(
    t: Result<&GitTransport, &String>,
    options: &TurnOptions,
    registry: &Registry,
    line: &str,
) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        // A malformed line has no id to answer against, so the only correct
        // response is none. Report it where a human will see it.
        Err(error) => {
            eprintln!("caos cc serve: ignoring unparseable request: {error}");
            return None;
        }
    };
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    id.as_ref()?;
    let id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => Some(reply(id, initialize(&params))),
        // Whatever has been found so far, which early on is nothing. The
        // client is told when that changes.
        "tools/list" => Some(match registry.lock() {
            Ok(registry) => reply(id, json!({ "tools": registry.clone() })),
            Err(_) => fail(id, -32603, "the tool registry lock is poisoned"),
        }),
        // A workspace we could not open is the model's problem to report, not
        // a protocol error: `isError` reaches the transcript, where a -32603
        // reaches a log nobody is reading.
        "tools/call" => Some(match t {
            Err(error) => reply(
                id,
                json!({
                    "content": [{ "type": "text", "text": format!(
                        "caos has no workspace, so no tool can run: {error}\n\
                         The tool server is started by Claude Code, so it looks for the \
                         repository at $CLAUDE_PROJECT_DIR and falls back to its working \
                         directory. Neither was a git working tree."
                    ) }],
                    "isError": true,
                }),
            ),
            Ok(t) => match call(t, options, &params) {
                Ok(result) => reply(id, result),
                // A tool that could not run at all is a JSON-RPC error; a tool
                // that ran and failed is a result with `isError`, which the
                // model sees and can act on. Conflating them hides real
                // breakage as advice.
                Err(error) => fail(id, -32603, &error),
            },
        }),
        "ping" => Some(reply(id, json!({}))),
        other => Some(fail(id, -32601, &format!("unknown method {other:?}"))),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    let version = match SUPPORTED.contains(&requested) {
        true => requested,
        false => PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        // `listChanged` is not decoration: this server answers `tools/list`
        // before it knows the answer, and the notification is how the real one
        // arrives. A client that ignores it sees the tools on its next listing.
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": { "name": "caos", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn call(t: &GitTransport, options: &TurnOptions, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call has no tool name".to_string())?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let session = args
        .get(SESSION_ARG)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "{SESSION_ARG} is missing: the PreToolUse hook that supplies it \
                 is not installed, so this call cannot be attributed to a conversation"
            )
        })?;
    let outcome = super::run_tool(t, options, session, name, &args)?;
    Ok(json!({
        "content": [{ "type": "text", "text": outcome.text }],
        "isError": outcome.is_error,
    }))
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn fail(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The tool registry Claude Code is given, from the step that implements it.
///
/// A LISTING IS A WORKER RUN. It resolves `--llm-step`, runs it, and reads the
/// declarations back, which is the point -- the tools are the step's, so a
/// session in any repository offers exactly what the tui offers there. A
/// failure leaves the session with no caos tools and the reason on stderr:
/// `tools/list` has nowhere to put an explanation, and a server that answers
/// with nothing is still a server that answers.
fn declarations(t: Result<&GitTransport, &String>, options: &TurnOptions) -> Vec<Value> {
    let registry = match t {
        Err(error) => Err(format!("no caos workspace: {error}")),
        Ok(t) => super::declarations(t, options),
    };
    match registry {
        Ok(registry) => registry.iter().map(mcp_declaration).collect(),
        Err(error) => {
            eprintln!("caos cc serve: cannot describe the caos tools: {error}");
            Vec::new()
        }
    }
}

/// One of the step's declarations, as MCP spells it: `inputSchema` rather than
/// the Anthropic API's `input_schema`, plus the arguments the `PreToolUse` hook
/// fills in. Those are DECLARED rather than smuggled, so the model's own call
/// stays schema-valid and the hook only supplies values the tool accepted.
fn mcp_declaration(declaration: &Value) -> Value {
    let mut schema = declaration
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {}, "required": [] }));
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for injected in [SESSION_ARG, "caos_tool_use_id", "caos_prompt_id"] {
            properties.insert(
                injected.to_string(),
                json!({
                    "type": "string",
                    "description": "Supplied automatically by the caos PreToolUse hook; do not set it.",
                }),
            );
        }
    }
    json!({
        "name": declaration.get("name").cloned().unwrap_or(Value::Null),
        "description": declaration.get("description").cloned().unwrap_or(Value::Null),
        "inputSchema": schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A notification carries no `id`, and JSON-RPC forbids answering one.
    /// Claude Code sends `notifications/initialized` immediately after the
    /// handshake, so getting this wrong breaks every session at startup.
    #[test]
    fn notifications_are_never_answered() {
        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let workspace = "no workspace".to_string();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        assert!(handle(
            Err(&workspace),
            &TurnOptions::default(),
            &registry,
            notification
        )
        .is_none());
    }

    #[test]
    fn an_unparseable_line_produces_no_response() {
        let workspace = "no workspace".to_string();
        let registry: Registry = Arc::new(Mutex::new(Vec::new()));
        assert!(handle(
            Err(&workspace),
            &TurnOptions::default(),
            &registry,
            "{not json"
        )
        .is_none());
    }

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

    /// Every tool must accept the injected session arg, or the hook's
    /// `updatedInput` would produce a call that fails schema validation.
    #[test]
    fn a_declaration_carries_the_injected_args() {
        let declared = mcp_declaration(&json!({
            "name": "read",
            "description": "Read a file.",
            "input_schema": {
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"],
            },
        }));
        let properties = &declared["inputSchema"]["properties"];
        assert!(properties.get(SESSION_ARG).is_some());
        assert!(properties.get("file_path").is_some());
        assert_eq!(declared["inputSchema"]["required"], json!(["file_path"]));
        assert!(declared.get("input_schema").is_none());
    }
}
