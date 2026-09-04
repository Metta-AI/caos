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

use serde_json::{json, Value};

use caos::GitTransport;

use super::tools;

/// The version we implement. A client that asks for another gets its own value
/// echoed back when we can speak it, per MCP's negotiation rule.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];

/// The arg the `PreToolUse` hook injects. It is declared in every schema rather
/// than smuggled in, so the model's own call is valid with or without it and
/// the hook is only supplying a value the tool always accepted.
const SESSION_ARG: &str = "caos_session";

/// The workspace is passed in UNRESOLVED, and a failure to open it does not
/// stop the server.
///
/// Exiting here was worse than useless: Claude Code reports a tool server that
/// dies before it speaks as `CONNECTION_CLOSED`, which says nothing about a
/// repository, a directory, or caos -- and the message it would have printed
/// goes wherever a dead child's stderr goes. Answering `initialize` and then
/// naming the problem on the first tool call puts the reason in front of the
/// person who can fix it.
pub fn serve(workspace: Result<GitTransport, String>) -> Result<(), String> {
    let workspace = match workspace {
        Ok(t) => Ok(t),
        Err(error) => {
            eprintln!("caos cc serve: cannot open the caos workspace: {error}");
            eprintln!("caos cc serve: serving anyway; tools will report this when called");
            Err(error)
        }
    };
    let t = workspace.as_ref();

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("reading request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle(t, &line) else {
            continue;
        };
        let encoded = serde_json::to_string(&response)
            .map_err(|error| format!("encoding response: {error}"))?;
        writeln!(stdout, "{encoded}").map_err(|error| format!("writing response: {error}"))?;
        stdout
            .flush()
            .map_err(|error| format!("flushing response: {error}"))?;
    }
    Ok(())
}

/// Handle one message. `None` means "say nothing", which is required rather
/// than merely polite: a JSON-RPC notification has no `id`, and answering one
/// is a protocol violation.
fn handle(t: Result<&GitTransport, &String>, line: &str) -> Option<Value> {
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
        "tools/list" => Some(reply(id, json!({ "tools": declarations(t) }))),
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
            Ok(t) => match call(t, &params) {
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
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "caos", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn call(t: &GitTransport, params: &Value) -> Result<Value, String> {
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
    match super::run_tool(t, session, name, &args) {
        Ok(text) => Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        })),
        Err(tools::ToolError::User(message)) => Ok(json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        })),
        Err(tools::ToolError::Infra(error)) => Err(error),
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn fail(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The tool registry. Descriptions carry the same guidance the worker's inline
/// tools give (`std/llm-step/src/tools.rs`), because a model should meet one
/// description of `edit` no matter which harness is running it.
fn declarations(t: Result<&GitTransport, &String>) -> Vec<Value> {
    let mut tools = vec![
        declaration(
            "read",
            "Read a file from the conversation workspace. The workspace is the \
             conversation's tree, not your checkout. Large files are truncated; \
             use offset/limit (line-based) to page.",
            json!({
                "file_path": { "type": "string", "description": "Workspace-relative path." },
                "offset": { "type": "integer", "description": "1-based first line to return." },
                "limit": { "type": "integer", "description": "Number of lines to return." },
            }),
            &["file_path"],
        ),
        declaration(
            "ls",
            "List a directory in the conversation workspace: one entry per line, \
             directories with a trailing `/`.",
            json!({
                "path": {
                    "type": "string",
                    "description": "Directory to list; omit for the workspace root.",
                },
            }),
            &[],
        ),
        declaration(
            "grep",
            "Search the conversation workspace with a regular expression (Rust \
             regex syntax, line-based). Returns `path:linenum:line`. Scope with \
             `path` to narrow it; results are cached per unchanged subtree, so \
             repeated and scoped searches are cheap. Prefer this over reading \
             files to look for something.",
            json!({
                "pattern": { "type": "string", "description": "The regular expression to search for." },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search; omit for the whole workspace.",
                },
            }),
            &["pattern"],
        ),
        declaration(
            "bash",
            "Run a shell command in the workspace (executed with `sh -c` from the \
             workspace root). Use this for COMMANDS (builds, tests, scripts); for \
             plain file access prefer read/ls/grep/write/edit, which are \
             immediate. The workspace is materialized lazily: ONLY the files and \
             directories you list in `paths` are readable — a command touching \
             any other existing path fails with 'Permission denied' (EACCES), \
             and the result names the unmaterialized paths it touched. When that \
             happens, retry the same command with those paths added to `paths`. \
             Creating new files or directories needs no declaration. The result \
             reports the exit code, stdout and stderr (tails), and the workspace \
             carries all changes forward. A non-zero exit is reported back to \
             you, not an error — read stderr and react.",
            json!({
                "cmd": { "type": "string", "description": "The shell command to run." },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Workspace-relative paths the command reads or modifies; \
                     only these are materialized into the sandbox.",
                },
            }),
            &["cmd"],
        ),
        declaration(
            "write",
            "Write a file into the conversation workspace, creating parent \
             directories and overwriting any existing file. An existing file \
             keeps its mode.",
            json!({
                "file_path": { "type": "string", "description": "Workspace-relative path." },
                "content": { "type": "string", "description": "The full new file content." },
            }),
            &["file_path", "content"],
        ),
        declaration(
            "edit",
            "Replace text in a workspace file. `old_string` must match exactly \
             and — unless `replace_all` — appear exactly once; include \
             surrounding context to disambiguate.",
            json!({
                "file_path": { "type": "string", "description": "Workspace-relative path." },
                "old_string": { "type": "string", "description": "Exact text to replace." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false).",
                },
            }),
            &["file_path", "old_string", "new_string"],
        ),
    ];
    // The harness's own std tools, described by the `help` their images carry
    // rather than by a copy of it here — the same source `llm-step` reads, so a
    // tool reads one way wherever it is offered.
    //
    // A tool that cannot be described is SKIPPED, not fatal: `tools/list` is
    // answered on every session start, and a std entry that fails to resolve
    // (a half-built tree, a server that is not up) should cost that one tool
    // rather than leaving the model with none at all.
    // Without a workspace there is nothing to resolve these against, and the
    // built-ins above are still worth declaring: a tools/list that answers is
    // what lets the session start and say why, rather than dying unexplained.
    if let Ok(t) = t {
        for name in ["caos-build", "caos-test", "caos-test-result"] {
            let Some(entry) = tools::std_tool_entry(name) else {
                continue;
            };
            // PRESENT BEFORE DESCRIBED, and this ordering is the whole point.
            // Describing one pushes the workspace tree to the caos server, and
            // `std_tool_entry` is a static name map that says yes in any
            // repository -- so an ordinary checkout with no `std/` was pushing
            // itself to caos three times at every session start, to describe
            // tools that cannot exist there.
            //
            // Worse than wasteful: it put a network round trip in front of the
            // MCP handshake. A caos server that REFUSES is harmless (the error
            // is caught and the tool skipped), but one reached through a tunnel
            // whose far end is gone does not refuse -- it accepts and swallows,
            // so the push hangs, `tools/list` never answers, and the session
            // reports a tool server that closed rather than one still waiting.
            if !t.work_dir().join(entry).is_dir() {
                continue;
            }
            match tools::describe_std_tool(t, entry) {
                Ok(help) => tools.push(std_declaration(name, &help)),
                Err(error) => eprintln!("caos cc serve: skipping {name}: {error:?}"),
            }
        }
    }
    tools
}

/// A std tool's registry entry, from its parsed help. Every declared parameter
/// is a string: an arg reaches a worker as a blob whatever JSON type it left the
/// model as, which is the same choice `tree_tool_declaration` makes.
fn std_declaration(name: &str, help: &tools::StdToolHelp) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in &help.params {
        properties.insert(
            param.name.clone(),
            json!({ "type": "string", "description": param.doc }),
        );
        if param.required {
            required.push(param.name.clone());
        }
    }
    let required: Vec<&str> = required.iter().map(String::as_str).collect();
    declaration(name, &help.doc, Value::Object(properties), &required)
}

fn declaration(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut properties = properties;
    for injected in [SESSION_ARG, "caos_tool_use_id", "caos_prompt_id"] {
        properties[injected] = json!({
            "type": "string",
            "description": "Supplied automatically by the caos PreToolUse hook; do not set it.",
        });
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> Option<GitTransport> {
        GitTransport::from_cwd().ok()
    }

    /// A notification carries no `id`, and JSON-RPC forbids answering one.
    /// Claude Code sends `notifications/initialized` immediately after the
    /// handshake, so getting this wrong breaks every session at startup.
    #[test]
    fn notifications_are_never_answered() {
        let Some(t) = transport() else { return };
        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle(Ok(&t), notification).is_none());
    }

    #[test]
    fn an_unparseable_line_produces_no_response() {
        let Some(t) = transport() else { return };
        assert!(handle(Ok(&t), "{not json").is_none());
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
    fn every_tool_declares_the_injected_session_arg() {
        let Some(t) = transport() else { return };
        for tool in declarations(Ok(&t)) {
            let properties = &tool["inputSchema"]["properties"];
            assert!(
                properties.get(SESSION_ARG).is_some(),
                "{} does not declare {SESSION_ARG}",
                tool["name"]
            );
            assert!(
                !tool["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(SESSION_ARG)),
                "{} requires {SESSION_ARG} of the model",
                tool["name"]
            );
        }
    }

    #[test]
    fn the_registry_covers_exactly_the_implemented_tools() {
        let Some(t) = transport() else { return };
        let names: Vec<String> = declarations(Ok(&t))
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        // The std tools are appended from their images, which needs a server;
        // the built-in half is fixed and is what this pins.
        assert_eq!(&names[..6], ["read", "ls", "grep", "bash", "write", "edit"]);
    }
}
