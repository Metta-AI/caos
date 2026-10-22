//! What a degraded `caos mcp` process can say about where it is running.
//!
//! One function answers what a failed hook or tool call needs to be diagnosed
//! after the fact: the environment (by name), the working directory, how the
//! process was started, and whether the container is new. Every failure path
//! calls it: the hooks, `help`, and the SessionStart hook (through
//! `caos mcp diag`).
//!
//! It reads facts and invents none: a missing file or a failed command becomes
//! a note, never an error, because the code that explains a broken session must
//! not break. Environment variables appear by NAME only (a `CAOS_*` value can
//! be a ticket), and the text goes through `redact_secrets` before it leaves.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Who is asking, and why.
#[derive(Default)]
pub struct Context<'a> {
    /// The hook event, or `help` / `serve` for the callers that are not
    /// hooks.
    pub event: &'a str,
    /// `startup` or `resume`, from a SessionStart payload.
    pub source: Option<&'a str>,
    pub session: Option<&'a str>,
    /// Why the caller is degraded, when it knows.
    pub reason: Option<&'a str>,
}

/// The facts, as text. Unredacted: the caller redacts the whole document it
/// assembles (twice would mangle the marker).
pub fn collect(ctx: &Context) -> String {
    let mut d = String::from("--- caos diag ---\n");
    d.push_str(&format!("event: {}\n", ctx.event));
    d.push_str(&format!("source: {}\n", ctx.source.unwrap_or("<none>")));
    d.push_str(&format!("session: {}\n", ctx.session.unwrap_or("<none>")));
    if let Some(reason) = ctx.reason {
        d.push_str(&format!("reason: {}\n", reason.replace('\n', " ")));
    }
    d.push_str(&format!(
        "client CAOS_REV: {}\n",
        std::env::var("CAOS_REV").unwrap_or_else(|_| "<unset>".to_string())
    ));
    d.push_str(&format!("claude code: {}\n", claude_version()));
    d.push_str(&format!("setup stamp mtime: {}\n", setup_stamp_age()));

    let cwd = std::env::current_dir();
    d.push_str(&format!(
        "cwd: {}\n",
        cwd.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unreadable: {e}>"))
    ));
    d.push_str(&format!(
        "CLAUDE_PROJECT_DIR: {}\n",
        std::env::var("CLAUDE_PROJECT_DIR").unwrap_or_else(|_| "<unset>".to_string())
    ));
    let home = std::env::var("HOME").ok();
    d.push_str(&format!(
        "HOME: {}\nuid: {}\n",
        home.as_deref().unwrap_or("<unset>"),
        uid()
    ));
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("CAOS_"))
        .collect();
    names.sort();
    d.push_str(&format!("CAOS_* variables set (names only): {names:?}\n"));

    d.push_str(&format!("server named by environment: {}\n", server_env()));
    d.push_str(&format!(
        "boot_id: {}\nuptime: {}\nhostname: {}\n",
        proc_line("/proc/sys/kernel/random/boot_id"),
        proc_line("/proc/uptime"),
        proc_line("/proc/sys/kernel/hostname"),
    ));
    d.push_str(&format!(
        "process start (clock ticks after boot): {}\n",
        start_ticks()
    ));
    d.push_str(&format!("process chain: {}\n", process_chain()));
    d
}

/// Collect, put it on stderr and in the append-only journal, and return the
/// redacted text.
///
/// STDERR is where a hook's events carry it; the JOURNAL is the copy that
/// survives the session ending. Its first line says whether this is the first
/// diag in the container, because a fresh container has an empty cache and so
/// the absence of earlier entries is itself evidence.
pub fn emit(ctx: &Context) -> String {
    let path = journal_path();
    let before = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let order = if before == 0 {
        "first diag in this container (the journal was empty or absent)".to_string()
    } else {
        format!("diag #{} in this container", before + 1)
    };
    let text = super::serve::redact_secrets(&format!("{order}\n{}", collect(ctx)));
    for line in text.lines() {
        eprintln!("caos diag: {line}");
    }
    if let Some(path) = path {
        append_journal(&path, ctx, &text);
    }
    text
}

fn journal_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/caos/diag.jsonl"))
}

fn append_journal(path: &Path, ctx: &Context, text: &str) {
    use std::io::Write as _;
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record: Value = json!({
        "at": at,
        "event": ctx.event,
        "source": ctx.source,
        "session": ctx.session,
        "text": text,
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{record}");
    }
}
/// Which of the variables that can name the server are set (names only: a value
/// can be a ticket).
fn server_env() -> String {
    let set: Vec<&str> = ["CAOS_SERVER_URL", "CAOS_MCP_DEFAULT_SERVER"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    if set.is_empty() {
        "none (only --server=<url> can have named it)".to_string()
    } else {
        set.join(", ")
    }
}

fn proc_line(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().to_string(),
        Err(error) => format!("<unreadable: {error}>"),
    }
}

fn uid() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
        })
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Field `index` (1-based, per proc(5)) of `/proc/<pid>/stat`. The command name
/// is parenthesised and may contain spaces, so count from its closing paren.
fn stat_field(pid: u32, index: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` begins at field 3 (state).
    rest.split_whitespace()
        .nth(index.checked_sub(3)?)
        .map(str::to_string)
}

fn start_ticks() -> String {
    stat_field(std::process::id(), 22).unwrap_or_else(|| "<unknown>".to_string())
}

fn process_chain() -> String {
    let mut chain = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..8 {
        let comm = proc_line(&format!("/proc/{pid}/comm"));
        chain.push(format!("{pid}:{comm}"));
        match stat_field(pid, 4).and_then(|p| p.parse::<u32>().ok()) {
            Some(parent) if parent > 0 => pid = parent,
            _ => break,
        }
    }
    chain.join(" <- ")
}

/// `claude --version`, bounded: a hung client must not hang the diagnosis.
fn claude_version() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("claude")
            .arg("--version")
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        Ok(Ok(out)) => format!("<exit {:?}>", out.status.code()),
        Ok(Err(error)) => format!("<not runnable: {error}>"),
        Err(_) => "<timed out>".to_string(),
    }
}

fn setup_stamp_age() -> String {
    let path = "/usr/local/share/caos/setup-stamp";
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => {
            let at = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("unix {at}")
        }
        Err(error) => format!("<unreadable: {error}>"),
    }
}

/// `caos mcp diag [--event=E] [--source=S] [--session=ID] [--reason=R] [--stdin]`
///
/// The entry for callers outside this process (the Go SessionStart hook), which
/// has no workspace to hand over. `--stdin` takes a hook payload and reads the
/// event, source and session from it, so the caller does not have to parse JSON.
pub fn cli(args: &[&str]) -> Result<(), String> {
    let mut event = String::from("diag");
    let mut source = None;
    let mut session = None;
    let mut reason = None;
    for arg in args {
        if let Some(v) = arg.strip_prefix("--event=") {
            event = v.to_string();
        } else if let Some(v) = arg.strip_prefix("--source=") {
            source = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--session=") {
            session = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--reason=") {
            reason = Some(v.to_string());
        } else if *arg == "--stdin" {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .map_err(|e| format!("reading payload: {e}"))?;
            if let Ok(payload) = serde_json::from_str::<Value>(&input) {
                let text = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
                event = text("hook_event_name").unwrap_or(event);
                source = text("source").or(source);
                session = text("session_id").or(session);
            }
        } else {
            return Err(format!("caos mcp diag: unknown argument {arg:?}"));
        }
    }
    emit(&Context {
        event: &event,
        source: source.as_deref(),
        session: session.as_deref(),
        reason: reason.as_deref(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_has_the_fields_and_no_env_values() {
        std::env::set_var("CAOS_DIAG_TEST_SECRET", "hunter2-value");
        let text = collect(&Context {
            event: "SessionStart",
            source: Some("resume"),
            session: Some("s1"),
            reason: Some("boom"),
        });
        for needle in [
            "event: SessionStart",
            "source: resume",
            "session: s1",
            "reason: boom",
            "cwd:",
            "CLAUDE_PROJECT_DIR:",
            "HOME:",
            "uid:",
            "server named by environment:",
            "boot_id:",
            "process chain:",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(text.contains("CAOS_DIAG_TEST_SECRET"), "names are listed");
        assert!(!text.contains("hunter2-value"), "values never are");
    }

    #[test]
    fn a_ticket_in_a_failure_reason_is_redacted() {
        let text = super::super::serve::redact_secrets(&collect(&Context {
            event: "x",
            reason: Some("remote caos://abcdefghijklmnopqrstuvwxyz0123456789SECRETTOKEN failed"),
            ..Context::default()
        }));
        assert!(!text.contains("SECRETTOKEN"), "{text}");
    }
}
