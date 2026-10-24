//! The tool server a harness spawns over stdio: a bridge to the caos server's
//! `POST /mcp`, which is where the protocol lives.
//!
//! Some harnesses can reach the server by URL and need no bridge at all. One
//! that can only reach it through `caos://` (a cloud container, which dials out
//! and listens for nothing) cannot speak that transport itself, so it runs this:
//! a line in on stdin is a JSON-RPC message, a line out is its answer, and the
//! bytes between are carried to the server by the client's own transport. The
//! tools, the instructions, the handshake and the result decoding are the
//! server's (`rust/crates/server/src/mcp.rs`); nothing about them is here.
//!
//! STDOUT CARRIES THE PROTOCOL. Nothing may print to it but a response -- a stray
//! `println!` desynchronizes the stream and the session dies with a parse error
//! naming neither this file nor the line that wrote. Diagnostics go to stderr.
//!
//! THREE THINGS ARE LOCAL:
//!
//! - **The handshake and the tool list are answered here, from constants**
//!   (`caos_world::mcp`, the same ones the server uses). A harness gives
//!   `initialize` a few seconds and drops a tool server slower than that from the
//!   session with no tools and no reason; one that crossed an iroh relay from a
//!   cold container lost that race.
//! - **A server that cannot be reached does not stop this process.** Claude Code
//!   reports a tool server that dies before it speaks as `CONNECTION_CLOSED`,
//!   which says nothing about a server, a locator or caos. Answering a call
//!   with the reason (and answering `help` with it) puts it in
//!   front of the person who can fix it.
//! - **`help` with no path gets this container's facts appended** (the client
//!   build, the dev stamp, whether git can reach a `caos://` remote). They are
//!   the only caos surface a locked-down cloud session has when the tools do not
//!   come up, and they describe the container, not the server.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::Config;

/// The step, as `POST /mcp` takes it in `X-Caos-Step`: the locator, or a hash.
fn step_header(config: &Config) -> Result<Option<String>, String> {
    let Some(argument) = config.step.as_deref() else {
        return Ok(None);
    };
    for prefix in [
        format!("--{}:@@=", crate::LLM_STEP_ARG),
        format!("--{}:hash=", crate::LLM_STEP_ARG),
    ] {
        if let Some(value) = argument.strip_prefix(&prefix) {
            return Ok(Some(value.to_string()));
        }
    }
    Err(format!(
        "{argument}: `caos mcp` reads no checkout, so the step is named by locator \
         (--{0}:@@=git+https://…?rev=<sha>&dir=std/{0}) or by hash (--{0}:hash=<oid>)",
        crate::LLM_STEP_ARG
    ))
}

pub fn serve(config: Config) -> Result<(), String> {
    let server = config.server();
    let step = step_header(&config);
    if let Some(error) = server.as_ref().err().or(step.as_ref().err()) {
        eprintln!("caos mcp serve: {error}");
        eprintln!("caos mcp serve: serving anyway; tools will report this when called");
        super::diag::emit(&super::diag::Context {
            event: "serve",
            reason: Some(error),
            ..Default::default()
        });
    }
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let secrets = caos::Secrets::current();

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("reading request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        // A notification (no `id`) is answered by nobody and means nothing to the
        // server: `notifications/initialized` arrives right behind the handshake,
        // and forwarding it would put a network round trip in front of the
        // `tools/list` queued behind it.
        if request_of(&line).is_none() {
            continue;
        }
        // The handshake and the list answer from constants, at once, with no
        // server: a harness gives `initialize` a few seconds and drops a tool
        // server slower than that without a word.
        if let Some((id, method, params)) = request_of(&line) {
            if let Some(answer) = caos_world::mcp::local_answer(&id, &method, &params) {
                write_message(&out, &answer)?;
                continue;
            }
        }
        let answer = match (&server, &step) {
            (Ok(server), Ok(step)) => caos::mcp_post(
                server,
                step.as_deref(),
                &secrets,
                config.max_result_chars,
                &line,
            ),
            (Err(error), _) | (_, Err(error)) => Err(error.clone()),
        };
        let response = match answer {
            Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(message) => Some(with_local_facts(&line, message)),
                Err(error) => Some(degraded(
                    &line,
                    &format!("the server's answer was not JSON: {error}"),
                )),
            },
            Ok(None) => None,
            Err(error) => degraded_or_none(&line, &error),
        };
        if let Some(response) = response {
            write_message(&out, &response)?;
        }
    }
    Ok(())
}

fn request_of(line: &str) -> Option<(Value, String, Value)> {
    let request: Value = serde_json::from_str(line).ok()?;
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str)?.to_string();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    Some((id, method, params))
}

/// Whether this request is `help` with no path: the overview.
fn is_overview(method: &str, params: &Value) -> bool {
    method == "tools/call"
        && params.get("name").and_then(Value::as_str) == Some("help")
        && params
            .get("arguments")
            .and_then(|args| args.get("path"))
            .and_then(Value::as_str)
            .is_none_or(|path| path.trim().is_empty())
}

/// The server's answer, with this container's facts appended when it is the
/// overview.
fn with_local_facts(line: &str, mut message: Value) -> Value {
    let Some((_, method, params)) = request_of(line) else {
        return message;
    };
    if !is_overview(&method, &params) {
        return message;
    }
    if let Some(text) = message.pointer_mut("/result/content/0/text") {
        if let Some(existing) = text.as_str() {
            *text = Value::String(format!("{existing}\n{}", local_facts(None)));
        }
    }
    message
}

fn local_facts(degraded: Option<&str>) -> String {
    let diag = super::diag::emit(&super::diag::Context {
        event: "help",
        reason: degraded,
        ..Default::default()
    });
    format!("{diag}\n{}", diagnostics())
}

/// No answer for a notification, a degraded one for anything with an id.
fn degraded_or_none(line: &str, error: &str) -> Option<Value> {
    request_of(line)?;
    Some(degraded(line, error))
}

/// What to say when the server cannot be asked: enough of a handshake to keep
/// the connection up, and one tool that gives the reason.
fn degraded(line: &str, error: &str) -> Value {
    let (id, method, params) =
        request_of(line).unwrap_or((Value::Null, String::new(), Value::Null));
    let reason =
        format!("caos is DEGRADED: it has no server, so no tool can run.\nreason: {error}\n");
    let overview =
        |text: String| json!({ "content": [{ "type": "text", "text": text }], "isError": true });
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    match method.as_str() {
        "tools/call" if is_overview(&method, &params) => {
            reply(overview(format!("{reason}\n{}", local_facts(Some(error)))))
        }
        "tools/call" => reply(overview(format!("{reason}`help` with no path says more."))),
        "ping" => reply(json!({})),
        other => json!({ "jsonrpc": "2.0", "id": id, "error": {
            "code": -32601, "message": format!("unknown method {other:?}") } }),
    }
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
