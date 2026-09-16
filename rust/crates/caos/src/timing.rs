//! A phase journal that outlives the process that wrote it.
//!
//! The expensive work in a Claude Code session is spread across processes that
//! nobody can watch: `mcp hook` fires per prompt and pushes the conversation
//! base, `mcp warm` resolves the tools, `mcp serve` answers `tools/list`. Each
//! measured its own phases into a per-PROCESS static that only its own later
//! code read -- so `ensure_code_commit`'s own doc comment can quote
//! `ensure-pushed 115.0s` while a session that just waited out that push has no
//! way to see it. A cloud session has no shell and no file tools, so "run it
//! again with a stopwatch" is not available either.
//!
//! So: one append-only file, written by whichever process did the work and read
//! back by `caos_status`. Best-effort throughout -- a phase journal that can
//! fail a run is worse than no phase journal, and this exists to explain slow
//! runs, not to gate them.

use std::io::Write;
use std::path::PathBuf;

/// Where the journal lives. `CAOS_TIMING_LOG` overrides it; setting it to an
/// empty value turns recording off.
///
/// It defaults to a path rather than to OFF because the sessions worth
/// profiling are the ones nobody thought to configure first: by the time a
/// cloud session is visibly slow, its container is gone, and a flag that had to
/// be set in advance would have been set after the fact. One line per phase
/// costs nothing.
fn journal() -> Option<PathBuf> {
    match std::env::var("CAOS_TIMING_LOG") {
        Ok(value) if value.is_empty() => None,
        Ok(value) => Some(PathBuf::from(value)),
        Err(_) => Some(std::env::temp_dir().join("caos-timings.log")),
    }
}

/// Seconds since the unix epoch, as a plain number.
///
/// Not a formatted date: this crate is the worker's `/bin/bash`-adjacent
/// binary and takes no dependency for a timestamp nobody reads as a date. The
/// only question asked of these is "how far apart", and subtraction answers it.
fn stamp() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Append one phase line. Never fails, never panics, never blocks on a lock.
///
/// `phase` names the work (`push`, `resolve-llm-step`); `detail` carries
/// whatever makes that line worth reading -- a duration, an object count, the
/// hash involved.
pub fn record(phase: &str, detail: &str) {
    let Some(path) = journal() else {
        return;
    };
    // TRUNCATED rather than rotated: this is a within-session journal and a
    // session is minutes long, so the only failure mode worth defending against
    // is a long-lived dev machine growing an unbounded file. A cap large enough
    // to hold any session's lines and small enough to never matter.
    const CAP: u64 = 256 * 1024;
    let over_cap = std::fs::metadata(&path)
        .map(|m| m.len() > CAP)
        .unwrap_or(false);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(!over_cap)
        .write(true)
        .truncate(over_cap)
        .open(&path)
    else {
        return;
    };
    let program = std::env::args().nth(1).unwrap_or_else(|| "caos".into());
    let _ = writeln!(
        file,
        "{:.3} {program}[{}] {phase}: {detail}",
        stamp(),
        std::process::id()
    );
}

/// Time `body`, record how long it took, and hand back its value untouched.
pub fn timed<T>(phase: &str, body: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let value = body();
    record(phase, &format!("{:.1}s", started.elapsed().as_secs_f64()));
    value
}

/// The journal's last `lines` lines, oldest first, for a reader that has no
/// shell. Each is rewritten with a RELATIVE timestamp -- seconds before the
/// most recent entry -- because the absolute epoch seconds are unreadable and
/// the only question is which step ate the wall clock.
pub fn tail(lines: usize) -> Vec<String> {
    let Some(path) = journal() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let kept = &all[all.len().saturating_sub(lines)..];
    let last = kept
        .last()
        .and_then(|line| line.split_whitespace().next())
        .and_then(|stamp| stamp.parse::<f64>().ok())
        .unwrap_or(0.0);
    kept.iter()
        .map(|line| match line.split_once(' ') {
            Some((at, rest)) => match at.parse::<f64>() {
                Ok(at) => format!("-{:>6.1}s {rest}", last - at),
                Err(_) => (*line).to_string(),
            },
            None => (*line).to_string(),
        })
        .collect()
}
