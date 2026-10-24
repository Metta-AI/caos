//! The MCP handshake and tool list, which every `caos` tool server answers the
//! same way and which depend on nothing: not a server, not a step, not a network.
//!
//! They live here, in the crate the client and server share, so the server's
//! `POST /mcp` and a stdio bridge's local answers cannot drift apart. The
//! bridge answers these itself, because a harness gives a tool server a few
//! seconds to complete `initialize` and drops one that is slower from the
//! session -- with no tools and no reason. A handshake that waited on the
//! network to the caos server (through an iroh relay, from a cold container)
//! lost that race, which is exactly what this being a constant avoids.

use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];

/// The arg the `PreToolUse` hook injects. It is declared in every schema rather
/// than smuggled in, so the model's own call is valid with or without it.
pub const SESSION_ARG: &str = "caos_session";

/// What the model is told about caos when the connection starts.
pub const INSTRUCTIONS: &str = "\
caos is a separate git workspace, reached only through these tools. It is not your \
local files: nothing moves between the two unless you copy it.

- help(path): what the tool at a path does and which arguments it takes. help() with \
no path says how caos is doing here.
- run(path, arguments): run the tool at a path and wait for its output. Arguments are \
strings (an array or object where the help says so).
- start(path, arguments), then result(ticket): the same without waiting. start returns a \
ticket; result says pending, or gives the output, or says how it failed.
- eval-path(path): evaluate a path and get the hash of what it produces. Pass the hash \
as `in` to read, ls or grep to look inside it.

A tool is a directory of the conversation. The standard ones are under caos-std/: read, \
ls, write, edit, copy, move, remove, grep, log, show, diff, merge, import-source, \
publish-source, bash-tool, caos-build, caos-test, github and more. caos-std exists only in \
the EVALUATED tree, so to see what is there: eval-path(\"caos-std\"), then run \
caos-std/read with file-path README.md and `in` set to that hash. Call help on a tool \
before you use one you have not used.

To work on code in caos, import it with import-source into a fresh path under imports/, \
copy it where you will change it, edit it with the file tools, and publish-source to push \
the result. Tools that act on a source tree take its path as `source-tree`.";

pub fn initialize(params: &Value) -> Value {
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
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "caos", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

pub fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn fail(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
/// The caos tools, as the model sees them.
pub fn tools() -> Vec<Value> {
    let path_and_arguments = |path_doc: &str| {
        with_injected(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": path_doc },
                "arguments": {
                    "type": "object",
                    "description": "The tool's arguments by name, as `help` lists them. \
                                    Strings, unless the help says an array."
                },
            },
            "required": ["path"],
        }))
    };
    vec![
        json!({
            "name": "run",
            "description": "Run the caos tool at a path and wait for its output. The path \
                            names a directory of the conversation, such as caos-std/bash-tool \
                            or caos-std/read; `help` at a path says what it takes. A tool \
                            that changes files changes the conversation's own copy, not your \
                            local checkout. For a long run use `start`.",
            "inputSchema": path_and_arguments(
                "Conversation path of the tool, such as caos-std/read or feature/01-change/caos-tools/test."
            ),
        }),
        json!({
            "name": "help",
            "description": "Describe the caos tool at a path: what it does and the arguments \
                            it takes. With no path, say how caos is doing in this session \
                            (the conversation, its head commit, and diagnostics) -- call this \
                            and report it verbatim if caos seems missing or broken.",
            "inputSchema": with_injected(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string",
                              "description": "Conversation path of the tool. Omit for the session overview." },
                },
                "required": [],
            })),
        }),
        json!({
            "name": "start",
            "description": "Start the caos tool at a path without waiting. Returns a ticket; \
                            ask `result` for it. The same call always has the same ticket, and \
                            starting one that has finished does not run it again.",
            "inputSchema": path_and_arguments(
                "Conversation path of the tool, such as caos-std/caos-test."
            ),
        }),
        json!({
            "name": "result",
            "description": "The state of a started call: still pending, its output, or how it \
                            failed. Asking never runs anything.",
            "inputSchema": with_injected(json!({
                "type": "object",
                "properties": {
                    "ticket": { "type": "string", "description": "The ticket `start` returned." },
                },
                "required": ["ticket"],
            })),
        }),
        json!({
            "name": "eval-path",
            "description": "Evaluate a path in the caos conversation and return the hash of \
                            what it produces: the build product of a tree, without having to \
                            know which tool makes it. caos-std exists only this way. Pass the \
                            hash as `in` to read, ls or grep to look inside it.",
            "inputSchema": with_injected(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string",
                              "description": "Path to evaluate, such as caos-std. Omit for the conversation root." },
                    "root": { "type": "string",
                              "description": "A tree or commit hash to evaluate within instead of the conversation; `path` is then relative to it." },
                },
                "required": [],
            })),
        }),
    ]
}

/// A schema that also accepts what the `PreToolUse` hook fills in. EVERY tool
/// goes through here: the hook injects into any call to this server, and a tool
/// whose schema does not declare those arguments gets an `updatedInput` that
/// fails validation, which reads as the model calling the tool wrongly.
pub fn with_injected(mut schema: Value) -> Value {
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
    schema
}

/// The answer to a message that needs nothing from the server, or `None` when
/// the message is one the server must answer (`tools/call`).
pub fn local_answer(id: &Value, method: &str, params: &Value) -> Option<Value> {
    match method {
        "initialize" => Some(reply(id.clone(), initialize(params))),
        "tools/list" => Some(reply(id.clone(), json!({ "tools": tools() }))),
        "ping" => Some(reply(id.clone(), json!({}))),
        _ => None,
    }
}
