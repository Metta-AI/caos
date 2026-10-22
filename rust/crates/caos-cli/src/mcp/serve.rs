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
//!
//! THE TOOLS ARE FIVE, AND THEY NEVER CHANGE: `run`, `help`, `start`, `result` and
//! `eval-path`. They are the server's own primitives (run something, describe it,
//! begin it without waiting, ask how it went, evaluate a path), and what they run
//! is named by a PATH in the conversation -- `caos-std/read`, `feature/01-change/
//! caos-tools/test` -- so there is nothing to list and nothing to resolve first.
//! A listing that depended on the step used to take a worker run, a cache, a
//! warm-up hook and a stand-in tool for when it failed; this one is a constant.
//! The model learns the rest from `instructions` (below), and from `help`.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use conversation_protocol::v3::refs;

use super::Config;

/// The version we implement. A client that asks for another gets its own value
/// echoed back when we can speak it, per MCP's negotiation rule.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];

/// The arg the `PreToolUse` hook injects. It is declared in every schema rather
/// than smuggled in, so the model's own call is valid with or without it and
/// the hook is only supplying a value the tool always accepted.
const SESSION_ARG: &str = "caos_session";

/// What the model is told about caos when the server starts. MCP carries this in
/// `initialize`, and a client puts it in front of the model, which is the one
/// place to say how the five tools fit together.
const INSTRUCTIONS: &str = "\
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

/// A server that cannot find caos does not stop.
///
/// Claude Code reports a tool server that dies before it speaks as
/// `CONNECTION_CLOSED`, which says nothing about a server, a locator or caos,
/// and whatever it printed goes wherever a dead child's stderr goes. Answering
/// `initialize` and then naming the problem on the first tool call puts the
/// reason in front of the person who can fix it.
pub fn serve(config: Config) -> Result<(), String> {
    if let Err(error) = config.server() {
        eprintln!("caos mcp serve: {error}");
        eprintln!("caos mcp serve: serving anyway; tools will report this when called");
        super::diag::emit(&super::diag::Context {
            event: "serve",
            reason: Some(&error),
            ..Default::default()
        });
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("reading request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle(&config, &line) else {
            continue;
        };
        write_message(&out, &response)?;
    }
    Ok(())
}

/// The caos tools, as the model sees them. Every one carries the arguments the
/// `PreToolUse` hook fills in (see [`with_injected`]).
fn tools() -> Vec<Value> {
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

/// The step's name for each of this server's tools. The step implements them
/// under the names it has always had; the server presents them under the names the
/// model reads.
fn step_tool(name: &str) -> Option<&'static str> {
    match name {
        "run" | "start" => Some("run_tool"),
        "help" => Some("tool_help"),
        "eval-path" => Some("eval_path"),
        _ => None,
    }
}

/// The conversation this session records into, and its head commit -- the hash
/// the `log` and `show` tools take. From inside a cloud session this is the only
/// way to learn it: the id is `cc/<Claude Code's internal session id>`, which
/// appears in no URL, so without this line finding it means reading the session's
/// events from outside with `drive --verb=conv`.
fn conversation_status(config: &Config, session: Option<&str>) -> String {
    let Some(session) = session else {
        return format!(
            "conversation: unknown -- this call carried no {SESSION_ARG}, so the \
             PreToolUse hook that supplies it is not installed\n"
        );
    };
    let id = match super::conversation_id_for(session) {
        Ok(id) => id,
        Err(error) => return format!("conversation: session {session:?} is unusable: {error}\n"),
    };
    let head = config
        .server()
        .and_then(|server| super::server_store(&server))
        .and_then(|store| store.read_ref(&refs::head_ref(&id)?));
    let head = match head {
        Ok(Some(head)) => head.as_str().to_string(),
        Ok(None) => "none yet -- no turn has been recorded".to_string(),
        Err(error) => format!("<unreadable: {error}>"),
    };
    format!("conversation: {id}\nconversation head: {head}\n")
}

/// Whether the server answers, for the overview: bounded, so a dead address is
/// reported in seconds rather than waited on. It says the same words a tool call
/// would fail with (`cannot reach the CAOS server`), because a model told only
/// "the call failed" cannot tell a dead server from a wrong tool.
fn server_status(config: &Config) -> String {
    match config
        .server()
        .and_then(|server| super::wait_server_reachable(&server))
    {
        Ok(()) => "server: reachable\n".to_string(),
        Err(error) => format!("server: {error}\n"),
    }
}

/// `help` with no path: how caos is doing here. A model that finds caos missing
/// should be able to ask and get the reason, not conclude caos is absent -- a
/// connected server with nothing to say looks, from inside the session, exactly
/// like no server at all.
fn overview(conversation: &str, degraded: Option<&str>) -> Value {
    // THE REASON LEADS. A degraded server has nothing to run, and the reader of
    // this text is usually a model deciding what to tell a person.
    let lead = match degraded {
        Some(reason) => {
            format!("caos is DEGRADED: it has no server, so no tool can run.\nreason: {reason}\n\n")
        }
        None => String::new(),
    };
    let conversation = redact_secrets(conversation);
    let diag = super::diag::emit(&super::diag::Context {
        event: "help",
        reason: degraded,
        ..Default::default()
    });
    let text = format!(
        "{lead}{INSTRUCTIONS}\n\n{conversation}{}\n{diag}",
        diagnostics()
    );
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

/// The container's provisioning facts, dumped into the status text.
///
/// This is the ONLY caos surface a locked-down cloud session has -- no shell, no
/// filesystem tools, and maybe no reachable server -- so when the workspace
/// tools don't come up, this is where "which client build am I actually
/// running, what server does it point at, and can git reach it" has to be
/// answerable. Every line is a plain read: a missing file or a failed command
/// becomes a note, never an error, because the one tool that explains a broken
/// session must not break.
fn diagnostics() -> String {
    let mut d = String::from("--- caos diagnostics ---\n");
    // The build THIS `mcp serve` binary is: the wrapper exports it (install.sh).
    // The one fact that settles "is the per-session refresh installing the
    // latest, or is a stale client frozen in?".
    d.push_str(&format!(
        "client CAOS_REV: {}\n",
        std::env::var("CAOS_REV").unwrap_or_else(|_| "<unset>".to_string())
    ));
    // WHETHER THIS SESSION IS IN DEV MODE, said outright rather than left to be
    // inferred from an absence. The failure it exists to name is picking the
    // wrong ENVIRONMENT: two of them look identical from inside, and a session
    // that quietly ran the pinned client presented as "my change did not take",
    // which cost a debugging session a single line here would have ended.
    //
    // FROM THE SETUP SCRIPT'S STAMP, because that is where dev mode happens:
    // `bootstrap.go --dev-commit=<sha>` takes the whole install package from a
    // caosd of one's own, and writes this file only once every step of it has
    // succeeded. The stamp carries no ticket -- a `caos://` URL is the
    // capability to drive that server, and this text is quoted verbatim into a
    // model's context.
    //
    // It used to read `CAOS_DEV`, from when a session HOOK did the overlay.
    // Nothing sets that variable any more, so the line said `off` in every dev
    // session it was asked about -- the exact wrong answer, in the one place a
    // locked-down session can look.
    //
    // BOTH SOURCES, because they can disagree and the disagreement is the
    // interesting state: the stamp says the environment was BUILT in dev mode,
    // a `dev-` revision says the client this server is running came from there.
    let dev_stamp = std::fs::read_to_string("/usr/local/share/caos/dev-stamp").unwrap_or_default();
    let stamped = |key: &str| -> Option<String> {
        dev_stamp
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    let dev_client = std::env::var("CAOS_REV")
        .map(|r| r.starts_with("dev-"))
        .unwrap_or(false);
    d.push_str(&format!(
        "dev mode: {}\n",
        match (stamped("rev"), dev_client) {
            (Some(rev), true) => format!(
                "on -- the whole install package came from refs/caos/dev at {}, \
                 and this conversation seeds from {} rather than from HEAD",
                &rev[..rev.len().min(12)],
                stamped("seed").map_or_else(
                    || "<none>".to_string(),
                    |c| c[..c.len().min(12)].to_string()
                )
            ),
            (Some(rev), false) => format!(
                "STAMPED BUT NOT RUNNING: this environment was built from \
                 refs/caos/dev at {}, yet the client serving this session is the \
                 pinned build -- something re-installed over it after setup",
                &rev[..rev.len().min(12)]
            ),
            (None, true) => "the client is a dev build, but this environment carries no dev \
                 stamp -- a snapshot older than the stamp, or a hand-placed binary"
                .to_string(),
            (None, false) => "off -- this session runs the caos its client repo pins. Pass \
                 the --dev-commit=<sha> that `caosd up --iroh` prints on the \
                 environment's setup line to run your own."
                .to_string(),
        }
    ));
    // What install.sh resolved this session -- repo, full commit, build tag.
    d.push_str("build record (/usr/local/share/caos/build):\n");
    d.push_str(&indent(&read_file("/usr/local/share/caos/build")));
    // Where the env came from, stamped once at setup.
    d.push_str("env stamp (/usr/local/share/caos/setup-stamp):\n");
    d.push_str(&indent(&read_file("/usr/local/share/caos/setup-stamp")));
    d.push_str(&format!("{}\n", crate::secret_store::reader_summary()));
    // Whether git can reach a `caos://` remote, which is a DIFFERENT question
    // from whether this client can: the client speaks the transport itself,
    // while git execs `git-remote-caos` by name for every push and fetch. A
    // session with the helper missing resolves its tools and then fails on the
    // first push with "git: 'remote-caos' is not a git command", which reads as
    // a git problem rather than an install one.
    d.push_str(&format!("git-remote-caos: {}\n", git_remote_helper()));
    // The CROSS-PROCESS phase journal, which is the only way this session can
    // see what the other caos processes did. `mcp hook` fires per prompt and
    // pays the conversation-base push. It
    // cannot print to a transcript, and a cloud session has no shell to go looking
    // with -- so it writes here and this reads it back.
    let phases = caos::timing::tail(25);
    if phases.is_empty() {
        d.push_str("phase journal: empty\n");
    } else {
        d.push_str("phase journal (most recent last, times relative to the newest):\n");
        for line in phases {
            d.push_str(&format!("  {line}\n"));
        }
    }
    // REDACTED ONCE, OVER THE WHOLE DOCUMENT, rather than field by field. A
    // `caos://` URL is the capability to drive that server and this text is
    // quoted verbatim by a model into a transcript; most of what is above comes
    // from OTHER processes -- the phase journal's git command lines, the hook
    // log, git's own errors -- which quote the remote URL in full and cannot be
    // asked not to. Redacting the assembled text is what makes that safe by
    // construction, so a field added later is covered without anyone
    // remembering. It is also why nothing below here re-redacts: applied twice,
    // the second pass mangles the first pass's marker.
    redact_secrets(&d)
}

/// Scrub anything a status must never surface.
///
/// A `caos://` URL IS A CREDENTIAL — it ends in the token that authorizes
/// driving that server (design/iroh-transport.md) — and this text is reported
/// VERBATIM by a model, into a transcript that may be pasted anywhere. So a
/// ticket is shown by its leading characters only: enough to tell two servers
/// apart and to confirm the remote is a ticket at all, never enough to use.
///
/// It replaces a scrub of `dumbpipe`'s `using secret key <64 hex>` line, which
/// was the same concern about the same kind of value.
pub(super) fn redact_secrets(s: &str) -> String {
    s.lines()
        .map(|line| match line.find(caos::TICKET_SCHEME) {
            Some(start) => {
                let rest = &line[start..];
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                // Enough of the endpoint to identify it; the token never.
                let shown = rest[..end.min(24)].to_string();
                format!(
                    "{}{shown}…<ticket redacted>{}",
                    &line[..start],
                    &rest[end..]
                )
            }
            None => match line.find("secret key") {
                Some(i) => format!("{}secret key <redacted>", &line[..i]),
                None => line.to_string(),
            },
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read a file for [`diagnostics`], trimmed, or a note on why not.
fn read_file(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim_end().to_string(),
        Err(e) => format!("<unreadable: {e}>"),
    }
}

/// Two-space-indent every line, or a placeholder for the empty string, so a
/// multi-line file reads as one block under its heading.
fn indent(s: &str) -> String {
    if s.is_empty() {
        return "  <empty>\n".to_string();
    }
    s.lines().map(|l| format!("  {l}\n")).collect::<String>()
}

/// Where git would find the `caos://` remote helper, or why it would not.
///
/// ASKED OF GIT, not of the filesystem: git resolves `git-remote-caos` by its
/// own rules (its exec path, then PATH), and the client puts its own directory
/// on PATH for exactly this. Answering from a directory listing could say
/// "present" for a helper git will not find.
fn git_remote_helper() -> String {
    match std::process::Command::new("git")
        .args(["--exec-path"])
        .output()
    {
        Ok(_) => {}
        Err(e) => return format!("<git failed: {e}>"),
    }
    let found = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("git-remote-caos"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_default();
    match found {
        Some(path) => path.display().to_string(),
        None => "<not on PATH: a caos:// remote cannot push or fetch>".to_string(),
    }
}

/// Stdout, behind its own lock, not stdout's: the protocol is one message per
/// line, and two writers interleaving mid-message would corrupt the stream that
/// `std::io::Stdout`'s internal lock protects
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

/// Handle one message. `None` means "say nothing", which is required rather
/// than merely polite: a JSON-RPC notification has no `id`, and answering one
/// is a protocol violation.
fn handle(config: &Config, line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        // A malformed line has no id to answer against, so the only correct
        // response is none. Report it where a human will see it.
        Err(error) => {
            eprintln!("caos mcp serve: ignoring unparseable request: {error}");
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
        // A constant: see the module docs.
        "tools/list" => Some(reply(id, json!({ "tools": tools() }))),
        // A server we cannot find is the model's problem to report, not a
        // protocol error: `isError` reaches the transcript, where a -32603
        // reaches a log nobody is reading.
        "tools/call" => Some({
            let is_overview = params.get("name").and_then(Value::as_str) == Some("help")
                && params
                    .get("arguments")
                    .and_then(|args| args.get("path"))
                    .and_then(Value::as_str)
                    .is_none_or(|path| path.trim().is_empty());
            match config.server() {
                Err(error) if is_overview => reply(id, overview("", Some(&error))),
                Err(error) => reply(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": format!(
                            "caos has no server, so no tool can run: {error}\n\
                             `help` with no path says more."
                        ) }],
                        "isError": true,
                    }),
                ),
                Ok(_) => match call(config, &params) {
                    Ok(result) => reply(id, result),
                    // A tool that could not run at all is a JSON-RPC error; a tool
                    // that ran and failed is a result with `isError`, which the
                    // model sees and can act on. Conflating them hides real
                    // breakage as advice.
                    Err(error) => fail(id, -32603, &error),
                },
            }
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
        // `listChanged` is false: the list is a constant, so this server never
        // revises it behind the client's back.
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "caos", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn call(config: &Config, params: &Value) -> Result<Value, String> {
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
        let status = format!(
            "{}{}",
            conversation_status(config, session),
            server_status(config)
        );
        return Ok(overview(&status, None));
    }
    let session = session.ok_or_else(|| {
        format!(
            "{SESSION_ARG} is missing: the PreToolUse hook that supplies it \
             is not installed, so this call cannot be attributed to a conversation"
        )
    })?;
    let outcome = match name {
        "start" => {
            let ticket = super::start_tool(config, session, "run_tool", &args)?;
            super::ToolOutcome {
                text: format!(
                    "ticket: {ticket}\n\nAsk for it with result(ticket=\"{ticket}\"). It is \
                     pending until the tool finishes."
                ),
                is_error: false,
            }
        }
        "result" => {
            let ticket = args
                .get("ticket")
                .and_then(Value::as_str)
                .ok_or_else(|| "result needs the `ticket` start returned".to_string())?;
            super::tool_result(config, ticket.trim())?
        }
        other => {
            let step = step_tool(other).ok_or_else(|| format!("unknown tool {other:?}"))?;
            super::run_tool(config, session, step, &args)?
        }
    };
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

/// A schema that also accepts what the `PreToolUse` hook fills in.
///
/// EVERY tool this server offers goes through here. The hook matches on the
/// `mcp__caos__` prefix, so it injects into any call to this server -- and a tool
/// whose schema does not declare those arguments gets an `updatedInput` that fails
/// validation, which reads as the model calling the tool wrongly. Declaring them
/// keeps the model's own call valid with or without them.
fn with_injected(mut schema: Value) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A notification carries no `id`, and JSON-RPC forbids answering one.
    /// Claude Code sends `notifications/initialized` immediately after the
    /// handshake, so getting this wrong breaks every session at startup.
    #[test]
    fn notifications_are_never_answered() {
        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle(&Config::default(), notification).is_none());
    }

    #[test]
    fn an_unparseable_line_produces_no_response() {
        assert!(handle(&Config::default(), "{not json").is_none());
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

    /// The model learns how caos fits together from `instructions`, so it must say
    /// where the tools are and how to find out more.
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
        let listed = handle(
            &Config::default(),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 5);
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

    /// `run`, `help` and `eval-path` are the step's `run_tool`, `tool_help` and
    /// `eval_path`; `start` begins the same call `run` does.
    #[test]
    fn the_tools_are_the_steps_under_the_names_the_model_reads() {
        assert_eq!(step_tool("run"), Some("run_tool"));
        assert_eq!(step_tool("start"), Some("run_tool"));
        assert_eq!(step_tool("help"), Some("tool_help"));
        assert_eq!(step_tool("eval-path"), Some("eval_path"));
        assert_eq!(step_tool("result"), None);
        assert_eq!(step_tool("read"), None);
    }

    /// With no server `help` still answers, and its first words are the reason.
    #[test]
    fn a_degraded_overview_leads_with_its_reason() {
        let result = overview("", Some("no caos server"));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("caos is DEGRADED"), "{text}");
        assert!(text.contains("reason: no caos server"), "{text}");
        assert!(text.contains("--- caos diag ---"), "{text}");
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn a_ticket_remote_is_reported_without_its_token() {
        let ticket = "caos://endpointabcq3jd3du66g5amur4rvkvqnwbvnais4wfpynkvcjo.\
                      8507a32d93dabb5d70a2a0d9631596413cbfb15988ed3e76a6417e4127f11b68";
        let shown = redact_secrets(&format!("caos remote: {ticket}"));
        // Identifiable — you can see it is a ticket, and which one.
        assert!(shown.starts_with("caos remote: caos://endpoint"), "{shown}");
        // But not usable: the token is the last field, and none of it survives.
        assert!(
            !shown.contains("8507a32d93dabb5d70a2a0d9631596413cbfb15988ed3e76a6417e4127f11b68"),
            "the token survived redaction: {shown}"
        );
        assert!(shown.contains("redacted"), "{shown}");
    }

    /// A ticket reaches this text from processes that do not know it is a
    /// secret: git quotes the remote URL in its own errors, and the phase
    /// journal carries the whole `git push` command line. Observed in a cloud
    /// session, whose agent redacted the leak by hand before reporting it.
    #[test]
    fn a_ticket_quoted_by_another_process_is_redacted_too() {
        let ticket = "caos://endpointabcq3jd3du66g5amur4rvkvqnwbvnais4wfpynkvcjo.\
                      8507a32d93dabb5d70a2a0d9631596413cbfb15988ed3e76a6417e4127f11b68";
        let journal = format!(
            "phase journal (most recent last):\n  \
             push-failed: 7662 after 0.6s: git push --progress {ticket} 7662:refs/caos/req/7662\n  \
             hook log: fatal: could not read from remote repository {ticket}\n"
        );
        let shown = redact_secrets(&journal);
        assert!(
            !shown.contains("8507a32d93dabb5d70a2a0d9631596413cbfb15988ed3e76a6417e4127f11b68"),
            "a token survived in a line this process did not write: {shown}"
        );
        assert_eq!(
            shown.matches("redacted").count(),
            2,
            "every occurrence must be redacted, not just the first: {shown}"
        );
    }

    #[test]
    fn an_http_remote_is_left_alone() {
        let line = "caos remote: http://localhost:9090";
        assert_eq!(redact_secrets(line), line);
    }
}
