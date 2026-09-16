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

/// The pause between `warm`'s resolve attempts. `mcp serve` itself no longer
/// retries -- it serves the registry `warm` cached, or resolves ONCE inline when
/// there is none (see `ensure_resolved`) -- but `warm`, which runs in the
/// session-start hook before the client starts, still retries through the setup
/// race until it succeeds or the hook's `timeout` stops it.
const RESOLVE_FAST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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
            eprintln!("caos mcp serve: cannot open the caos workspace: {error}");
            eprintln!("caos mcp serve: serving anyway; tools will report this when called");
            Err(error)
        }
    };
    let t = workspace.as_ref();
    let registry: Registry = Arc::new(Mutex::new(Found::default()));
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));

    // The registry `mcp warm` cached in the session-start hook, BEFORE the client
    // started -- the whole point of warm, and now the whole story: with it,
    // `tools/list` is answered from the first read. Without it (a dev checkout,
    // or a warm that could not finish), the first `tools/list` or `caos_status`
    // resolves ONCE, inline (`ensure_resolved`). Either way the answer is
    // whatever resolution concludes -- no background retry loop, no timed hold,
    // no `tools/list_changed` correcting it later.
    if let Ok(t) = t {
        if let Some(tools) = read_cached_registry(t, &options) {
            publish(&registry, tools);
        }
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("reading request: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle(t, &options, &registry, &line) else {
            continue;
        };
        write_message(&out, &response)?;
    }
    Ok(())
}

/// Resolve the tools once, up front, and leave them in the on-disk cache that
/// the `mcp serve` spawned moments later reads at startup.
///
/// This is what lets a session be ready on TURN ONE. `mcp serve` cannot answer
/// `initialize` and go build an image before the client's first `tools/list`, so
/// without a cache that first `tools/list` blocks on the resolve -- fine once the
/// step is built, but the first time in a tree it is a rustc compile measured in
/// minutes. Run from the session-start hook, which BLOCKS until it returns, this
/// moves the resolve to before Claude Code is even launched, so the list `cc
/// serve` answers is already known and instant.
///
/// NON-FATAL by contract. It always returns `Ok`, because the hook must not fail
/// a session over a cold cache: a warm that cannot reach the server yet, or a
/// resolve that errors, simply leaves no cache and `mcp serve` resolves inline on
/// the first `tools/list` exactly as it would have without this.
pub fn warm(t: &GitTransport, options: &TurnOptions) -> Result<(), String> {
    // RETRIED, because this races the session's own setup. The first attempt
    // commonly fails -- the server a moment from answering, a `caos` remote a
    // moment from being added -- and a single-shot warm that gave up there would
    // cache nothing and leave that first `tools/list` to block on the resolve,
    // which is the whole thing this exists to pre-empt. So keep trying until one
    // succeeds or the outer `timeout` in the hook kills us; a reachability probe
    // with a deadline sits in front so an unreachable server is a short failure,
    // not a swallowed one.
    // The real stop is the hook's `timeout`; this bound just keeps the loop
    // finite. Each attempt is a reachability probe plus a full resolve, so even
    // at a second of sleep between them the count is reached only if every
    // resolve is returning fast failures -- exactly the setup-race case worth
    // retrying through.
    const WARM_ATTEMPTS: u32 = 120;
    let mut last = "not started".to_string();
    for attempt in 0..WARM_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(RESOLVE_FAST_INTERVAL);
        }
        // TIMED, because the probe has a fixed budget and the question a failed
        // attempt raises is whether the dial was slow or the server was absent.
        // Those want opposite fixes and the message alone cannot tell them apart.
        let probe = std::time::Instant::now();
        if let Err(error) = t.ensure_server_reachable() {
            last = format!(
                "server not reachable after {:.1}s: {error}",
                probe.elapsed().as_secs_f64()
            );
            eprintln!(
                "caos mcp warm: attempt {} did not cache: {last}",
                attempt + 1
            );
            continue;
        }
        match declarations(t, options) {
            Ok(found) if !found.is_empty() => {
                write_cached_registry(t, options, &found);
                // WHERE the wait went, in the hook's own log. `declarations`
                // measures its phases into a per-PROCESS static that only
                // `serve`'s diagnostics read -- so a warm that took a minute
                // reported the minute and threw the breakdown away, and the one
                // session state where you need it (a slow start you are trying
                // to explain) is the one with no `serve` resolve to print it.
                let breakdown = super::discovery_timing()
                    .map(|timing| format!(": {timing}"))
                    .unwrap_or_default();
                eprintln!(
                    "caos mcp warm: cached {} tools for the first turn (attempt {}){breakdown}",
                    found.len(),
                    attempt + 1
                );
                return Ok(());
            }
            Ok(_) => last = "the step answered with no tools".to_string(),
            Err(error) => last = error,
        }
        eprintln!(
            "caos mcp warm: attempt {} did not cache: {last}",
            attempt + 1
        );
    }
    eprintln!("caos mcp warm: gave up ({last}); mcp serve will resolve on first use");
    Ok(())
}

/// The per-checkout file the resolved tool registry is cached in. `mcp warm` and
/// the `mcp serve` that follows it both open the same checkout, so both derive
/// this path from the git directory without one having to tell the other.
fn registry_cache_path(t: &GitTransport) -> std::path::PathBuf {
    t.git_dir().join("caos-cc-registry.json")
}

/// The cache the previous function's path holds, IF it is for the step this
/// server was configured with. Keyed by `--llm-step` so a client rebuilt to
/// drive a different step cannot be handed the old step's tools; a mismatch or
/// an empty list reads as "no cache" and the caller resolves from scratch.
fn read_cached_registry(t: &GitTransport, options: &TurnOptions) -> Option<Vec<Value>> {
    let bytes = std::fs::read(registry_cache_path(t)).ok()?;
    let cached: Value = serde_json::from_slice(&bytes).ok()?;
    if cached.get("llm_step").and_then(Value::as_str) != options.llm_step.as_deref() {
        return None;
    }
    let tools = cached.get("tools")?.as_array()?.clone();
    if tools.is_empty() {
        None
    } else {
        Some(tools)
    }
}

/// Write the registry cache. Best-effort and via a temp-then-rename, so a `cc
/// serve` reading it concurrently sees either the old file or the new one whole,
/// never a half-written one; a failure to write just means the next server
/// resolves from scratch, which is the behaviour before any cache existed.
fn write_cached_registry(t: &GitTransport, options: &TurnOptions, tools: &[Value]) {
    let payload = json!({ "llm_step": options.llm_step, "tools": tools });
    let Ok(bytes) = serde_json::to_vec(&payload) else {
        return;
    };
    let path = registry_cache_path(t);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The tools this server offers, and what it would say about them. Populated by
/// `warm`'s cache at startup, or by the inline resolve on the first `tools/list`
/// (`ensure_resolved`); empty until one of those has run.
type Registry = Arc<Mutex<Found>>;

#[derive(Default)]
struct Found {
    tools: Vec<Value>,
    /// What happened while looking, in the words the reader needs. Read out by
    /// [`STATUS_TOOL`] and by nothing else.
    status: Option<String>,
}

/// The one tool this server implements itself, offered ONLY while it has no
/// others -- and it exists because of how the alternative failed.
///
/// A model that is handed zero tools does not report "my tool server has no
/// tools". It reports that caos is not there, because from inside the session
/// those are the same observation: a connected server with an empty list is
/// indistinguishable from an absent one through anything the model can see.
/// That cost a day of relaying container output by hand.
///
/// So an empty registry is not silence. It is one tool whose description says
/// the tools are still coming and whose result says how it is going.
const STATUS_TOOL: &str = "caos_status";

fn status_declaration() -> Value {
    json!({
        "name": STATUS_TOOL,
        "description": "caos provisioning diagnostics: this session's client build, env \
                        stamp, `caos` remote, git remote helper, and the tool-resolution \
                        status. Always available. If the other caos workspace tools are \
                        MISSING, call this and report its FULL output verbatim rather than \
                        concluding caos is absent -- it says why (and whether they are \
                        still resolving). It takes no arguments and changes nothing.",
        "inputSchema": with_injected(json!({
            "type": "object", "properties": {}, "required": [],
        })),
    })
}

/// Resolve the tools ONCE, inline, if it has not happened yet -- then return, so
/// the caller reads a settled registry. This is what `tools/list` and
/// `caos_status` call before answering when `warm` left no cache.
///
/// Idempotent and single-shot: a registry that already holds tools (from the
/// cache or a prior resolve) or a recorded failure is left alone. The handler
/// loop is single-threaded, so no two requests race this. It BLOCKS the caller
/// for exactly one resolution -- a `caos` server round trip, and at most a cold
/// image build -- rather than a fixed hold; there is no retry, because `warm`
/// already retried through the setup race before the client ever started, and a
/// resolve that fails here fails for a reason `caos_status` then names.
fn ensure_resolved(options: &TurnOptions, registry: &Registry) {
    if let Ok(found) = registry.lock() {
        if !found.tools.is_empty() || found.status.is_some() {
            return;
        }
    }
    let outcome = resolve_once(options);
    if let Ok(mut found) = registry.lock() {
        match outcome {
            Ok(tools) => {
                found.tools = tools;
                found.status = None;
            }
            Err(reason) => found.status = Some(reason),
        }
    }
}

/// One resolution attempt: open the workspace, prove the server is reachable
/// (a bounded probe, so an unreachable server is named rather than swallowed), then ask
/// the step for its tools.
///
/// It does NOT write the on-disk cache -- `warm` owns that. A serve resolving
/// here is the fallback for a session `warm` did not reach, and persisting its
/// result would outlive the session's server: a later serve in the same checkout
/// would then report the cached tools even against a server that has since gone,
/// hiding exactly the unreachable-server failure `caos_status` exists to name.
fn resolve_once(options: &TurnOptions) -> Result<Vec<Value>, String> {
    let t = GitTransport::from_cwd().map_err(|e| format!("cannot open the caos workspace: {e}"))?;
    t.ensure_server_reachable()?;
    declarations(&t, options)
}

/// The `tools/list` result: the resolved tools, plus the `caos_status` stand-in.
fn tools_list_reply(id: Value, registry: &Registry) -> Value {
    match registry.lock() {
        Ok(found) => {
            // ALWAYS include `caos_status`, resolved or not. It used to be the
            // stand-in offered ONLY while the real tools were missing, so it
            // vanished the moment they resolved -- which meant its provisioning
            // diagnostics (build, remote, helper) were unreachable in the one
            // session state where you might still want them: a working one you
            // are trying to confirm. It carries no session and records nothing,
            // so it is a harmless read to keep beside the workspace tools.
            let mut tools = found.tools.clone();
            tools.push(status_declaration());
            reply(id, json!({ "tools": tools }))
        }
        Err(_) => fail(id, -32603, "the tool registry lock is poisoned"),
    }
}

fn status_result(registry: &Registry) -> Value {
    let text = match registry.lock() {
        Err(_) => "the caos tool registry lock is poisoned; this server is broken".to_string(),
        Ok(found) => match (&found.status, found.tools.is_empty()) {
            (_, false) => format!("{} caos tools are available.", found.tools.len()),
            (Some(status), true) => status.clone(),
            (None, true) => "the caos tools have not been resolved yet; call tools/list \
                             (or any caos tool) to resolve them."
                .to_string(),
        },
    };
    let text = format!("{text}\n{}", diagnostics());
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
    // What install.sh resolved this session -- repo, full commit, build tag.
    d.push_str("build record (/usr/local/share/caos/build):\n");
    d.push_str(&indent(&read_file("/usr/local/share/caos/build")));
    // Where the env came from, stamped once at setup.
    d.push_str("env stamp (/usr/local/share/caos/setup-stamp):\n");
    d.push_str(&indent(&read_file("/usr/local/share/caos/setup-stamp")));
    // WHAT THE ENVIRONMENT NAMED, before what the repo got. A missing `caos`
    // remote is nearly always one of these two lines: nothing named a server, or
    // the variable that names one has moved and the environment still sets the
    // old one. That failure is otherwise explained only in a hook log, which a
    // cloud session cannot read -- this tool is the surface it has.
    d.push_str(&format!(
        "CAOS_SERVER_URL: {}\n",
        match std::env::var("CAOS_SERVER_URL") {
            Ok(url) => redact_secrets(&url),
            Err(_) => "<unset>".to_string(),
        }
    ));
    if std::env::var_os("CAOS_IROH_TICKET").is_some() {
        d.push_str(
            "CAOS_IROH_TICKET: set, and nothing reads it \
             -- set CAOS_SERVER_URL to the ticket instead\n",
        );
    }
    // The remote the client dials -- present means session-start added it.
    // REDACTED, because a `caos://` remote is the capability itself and this
    // output is quoted verbatim by a model.
    d.push_str(&format!(
        "caos remote: {}\n",
        redact_secrets(&caos_remote())
    ));
    // Whether git can reach a `caos://` remote, which is a DIFFERENT question
    // from whether this client can: the client speaks the transport itself,
    // while git execs `git-remote-caos` by name for every push and fetch. A
    // session with the helper missing resolves its tools and then fails on the
    // first push with "git: 'remote-caos' is not a git command", which reads as
    // a git problem rather than an install one.
    d.push_str(&format!("git-remote-caos: {}\n", git_remote_helper()));
    // Where the discovery time went, once a resolve has succeeded: the phases of
    // the one resolution, AFTER the server was reachable (the reachability probe
    // is separate). Blank until a resolve has run.
    if let Some(timing) = super::discovery_timing() {
        d.push_str(&format!("tool discovery: {timing}\n"));
    }
    // The CROSS-PROCESS phase journal, which is the only way this session can
    // see what the other caos processes did. `mcp hook` fires per prompt and
    // pays the conversation-base push; `mcp warm` resolves the tools. Neither
    // can print to a transcript, and a cloud session has no shell to go looking
    // with -- so they write here and this reads it back.
    let phases = caos::timing::tail(25);
    if phases.is_empty() {
        d.push_str("phase journal: empty\n");
    } else {
        d.push_str("phase journal (most recent last, times relative to the newest):\n");
        for line in phases {
            d.push_str(&format!("  {line}\n"));
        }
    }
    // The warm step's own log: session-start runs `mcp warm` before the client
    // starts, and this is where it says whether it resolved and cached the
    // tools or why it could not -- the reason a first turn does or does not
    // already have them.
    d.push_str("warm log (/tmp/caos-warm.log):\n");
    d.push_str(&indent(&tail(&read_file("/tmp/caos-warm.log"), 10)));
    // Whether a warm cache was on disk for THIS serve to load at startup.
    d.push_str(&format!("registry cache: {}\n", registry_cache_state()));
    d
}

/// A one-line note on the on-disk tool-registry cache: present with how many
/// tools and for which step, or why not. `mcp warm` writes it and `mcp serve`
/// loads it (see [`registry_cache_path`]); a first turn with no tools when this
/// says "absent" means the warm did not finish, and when it says "present" means
/// the load key did not match.
fn registry_cache_state() -> String {
    let mut cmd = std::process::Command::new("git");
    if let Ok(dir) = std::env::var("CLAUDE_PROJECT_DIR") {
        cmd.args(["-C", &dir]);
    }
    let git_dir = match cmd.args(["rev-parse", "--absolute-git-dir"]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return "<no git dir to look in>".to_string(),
    };
    let path = std::path::Path::new(&git_dir).join("caos-cc-registry.json");
    match std::fs::read(&path) {
        Err(e) => format!("absent ({e}) at {}", path.display()),
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => {
                let n = v.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
                let step = v
                    .get("llm_step")
                    .and_then(Value::as_str)
                    .unwrap_or("<none>");
                format!("present: {n} tools, step {step}")
            }
            Err(e) => format!("present but unparseable ({e})"),
        },
    }
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
fn redact_secrets(s: &str) -> String {
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

/// The last `n` lines of `s`.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Two-space-indent every line, or a placeholder for the empty string, so a
/// multi-line file reads as one block under its heading.
fn indent(s: &str) -> String {
    if s.is_empty() {
        return "  <empty>\n".to_string();
    }
    s.lines().map(|l| format!("  {l}\n")).collect::<String>()
}

/// The URL of the `caos` git remote, read from the project the hook names (a
/// tool server's cwd is not contractually the repo -- see `run_tool`).
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

fn caos_remote() -> String {
    let mut cmd = std::process::Command::new("git");
    if let Ok(dir) = std::env::var("CLAUDE_PROJECT_DIR") {
        cmd.args(["-C", &dir]);
    }
    match cmd.args(["remote", "get-url", "caos"]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Ok(out) => format!("<none: {}>", String::from_utf8_lossy(&out.stderr).trim()),
        Err(e) => format!("<git failed: {e}>"),
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

/// Publish what was found, and stop describing the search.
fn publish(registry: &Registry, tools: Vec<Value>) {
    match registry.lock() {
        Ok(mut found) => {
            found.tools = tools;
            found.status = None;
        }
        Err(_) => eprintln!("caos mcp serve: the tool registry lock is poisoned"),
    }
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
        // The tools. `warm` normally cached them before the client started, so
        // this is a read. When it did not, the FIRST `tools/list` resolves them
        // once, inline, and answers with the result -- real tools, or (on a
        // failure) the `caos_status` stand-in that says why. No hold on a fixed
        // clock, no `tools/list_changed` correction later: the answer to this one
        // read is the settled answer, which is what a client that reads the list
        // exactly once (the mounted Claude Code) needs and what makes the timing
        // predictable. It can block for one resolution -- a server round trip,
        // at most a cold build -- but never for a retry loop.
        "tools/list" => {
            ensure_resolved(options, registry);
            Some(tools_list_reply(id, registry))
        }
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
            Ok(t) => match call(t, options, registry, &params) {
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
        // `listChanged` is false: `tools/list` now resolves before it answers, so
        // its reply is final and this server never revises the list behind the
        // client's back. Advertising the capability would promise a notification
        // that no longer comes.
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "caos", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn call(
    t: &GitTransport,
    options: &TurnOptions,
    registry: &Registry,
    params: &Value,
) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call has no tool name".to_string())?;
    // This server's own tool, and the one call that belongs to no conversation:
    // it reports on the server, so it takes no session and records nothing.
    if name == STATUS_TOOL {
        // Resolve first, so a status called before any `tools/list` (a client
        // that opens with a diagnostic) still reports what happened rather than
        // "not resolved yet" -- the unreachable-server case names the server here.
        ensure_resolved(options, registry);
        return Ok(status_result(registry));
    }
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
/// session in any repository offers exactly what the tui offers there.
///
/// The error is RETURNED rather than logged, because the caller has somewhere
/// to put it: the status this server reports when it has no tools. A reason
/// that reaches only stderr reaches nobody who is asking.
fn declarations(t: &GitTransport, options: &TurnOptions) -> Result<Vec<Value>, String> {
    Ok(super::declarations(t, options)?
        .iter()
        .map(mcp_declaration)
        .collect())
}

/// One of the step's declarations, as MCP spells it: `inputSchema` rather than
/// the Anthropic API's `input_schema`, plus the arguments the `PreToolUse` hook
/// fills in. Those are DECLARED rather than smuggled, so the model's own call
/// stays schema-valid and the hook only supplies values the tool accepted.
fn mcp_declaration(declaration: &Value) -> Value {
    let schema = declaration
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {}, "required": [] }));
    json!({
        "name": declaration.get("name").cloned().unwrap_or(Value::Null),
        "description": declaration.get("description").cloned().unwrap_or(Value::Null),
        "inputSchema": with_injected(schema),
    })
}

/// A schema that also accepts what the `PreToolUse` hook fills in.
///
/// EVERY tool this server offers goes through here, `caos_status` included.
/// The hook matches on the `mcp__caos__` prefix, so it injects into any call to
/// this server -- and a tool whose schema does not declare those arguments gets
/// an `updatedInput` that fails validation, which reads as the model calling
/// the tool wrongly. Declaring them keeps the model's own call valid with or
/// without them.
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
        let workspace = "no workspace".to_string();
        let registry: Registry = Arc::new(Mutex::new(Found::default()));
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
        let registry: Registry = Arc::new(Mutex::new(Found::default()));
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

    /// Including this server's OWN tool. The `PreToolUse` hook matches on the
    /// `mcp__caos__` prefix, so it injects into a `caos_status` call too, and a
    /// schema that did not declare those arguments would fail validation on the
    /// one call whose whole job is to explain why the others are missing.
    #[test]
    fn the_status_tool_carries_the_injected_args_too() {
        let properties = &status_declaration()["inputSchema"]["properties"];
        for injected in [SESSION_ARG, "caos_tool_use_id", "caos_prompt_id"] {
            assert!(
                properties.get(injected).is_some(),
                "{STATUS_TOOL} does not declare {injected}"
            );
        }
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

    #[test]
    fn an_http_remote_is_left_alone() {
        let line = "caos remote: http://localhost:9090";
        assert_eq!(redact_secrets(line), line);
    }
}

#[cfg(test)]
mod env_diagnostic_tests {
    /// The status text must NAME the variable that moved, because a cloud
    /// session cannot read the hook's log and this tool is all it has. A
    /// misconfigured environment reported only as "no caos remote" sends whoever
    /// is debugging it looking at the repository instead of the env.
    #[test]
    fn the_diagnostics_mention_both_server_variables() {
        let source = include_str!("serve.rs");
        assert!(
            source.contains("CAOS_SERVER_URL: {}"),
            "the status text no longer reports CAOS_SERVER_URL"
        );
        assert!(
            source.contains("CAOS_IROH_TICKET: set, and nothing reads it"),
            "the status text no longer names the variable that moved"
        );
    }
}
