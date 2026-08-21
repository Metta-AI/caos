//! The live work display: what a `run` or `run-tool` is doing while you wait.
//!
//! A run blocks on one `GET /run`, so there is nothing to see from the request
//! itself. This polls `GET /status/<arg tree>` on a second connection and draws
//! the tree the server reports (SPEC.md "Tracing").
//!
//! **Only on a terminal.** Not a stylistic choice: the suite runs 29 clients at
//! once, none of them attached to a tty, and a poll per client per interval
//! would be load the run does not need — plus redraw escapes in a captured log
//! make the log unreadable. Off the terminal this costs one `is_terminal` call
//! and starts no thread at all.

use std::io::{IsTerminal, Write};

use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often the display refreshes. Fast enough to feel live, slow enough that
/// the work — not the watching — is what the server is busy with.
const POLL: Duration = Duration::from_millis(400);

/// Most nodes drawn before the rest are summarised. A wide map (the suite's
/// 29-way fan-out) would otherwise scroll the display off the screen, which
/// makes it worse than no display.
const MAX_LINES: usize = 16;

/// One node of the server's `/status` JSON.
///
/// Read out of a `serde_json::Value` rather than derived: this crate carries
/// `serde_json` but not `serde`, and a new crates.io dependency here means a
/// bake-anchor change (`lint-bake-anchor.sh`) for four fields.
struct Node {
    name: String,
    requested: Option<u64>,
    started: Option<u64>,
    children: Vec<Node>,
}

impl Node {
    /// Missing or oddly-typed fields become absent rather than an error: the
    /// display renders whatever the server could tell it.
    fn parse(value: &serde_json::Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        Some(Self {
            name: value["name"].as_str().unwrap_or("(unnamed)").to_string(),
            requested: value["requested"].as_u64(),
            started: value["started"].as_u64(),
            children: value["children"]
                .as_array()
                .map(|kids| kids.iter().filter_map(Node::parse).collect())
                .unwrap_or_default(),
        })
    }
}

/// A running display. Dropping it stops the poller and clears what it drew.
pub(crate) struct Watch {
    /// Held only to be dropped: closing it disconnects the poller's receiver,
    /// which is how the sleep is CUT SHORT rather than waited out. A flag the
    /// thread checked after `sleep(POLL)` would add most of a poll interval to
    /// the end of every interactive run, which is a tax on exactly the runs
    /// short enough for it to be noticeable.
    _stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watch {
    /// Start watching `arg_tree`, or return an inert handle when there is no
    /// terminal to draw on.
    pub(crate) fn start(server: &str, arg_tree: &str) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self {
                _stop: None,
                thread: None,
            };
        }
        let (stop, wake) = mpsc::channel::<()>();
        let (server, arg_tree) = (server.to_string(), arg_tree.to_string());
        let thread = std::thread::spawn(move || {
            let mut drawn = 0usize;
            // The sleep IS the wait for the stop signal, so it ends the instant
            // the run does. A first draw is deliberately one interval away: a
            // run served from cache finishes before it, and flashing a tree at
            // someone for 40ms is worse than showing nothing.
            while wake.recv_timeout(POLL) == Err(mpsc::RecvTimeoutError::Timeout) {
                // A failed poll draws nothing and does not retry harder. The
                // display is a courtesy; the run it is watching is the point,
                // and a server too busy to answer /status is not a reason to
                // start complaining into the middle of someone's output.
                let Ok(Some(json)) = fetch(&server, &arg_tree) else {
                    continue;
                };
                let Some(root) = serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .as_ref()
                    .and_then(Node::parse)
                else {
                    continue;
                };
                let mut lines = Vec::new();
                render(&root, 0, &mut lines);
                drawn = draw(drawn, &lines);
            }
            clear(drawn);
        });
        Self {
            _stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // Sender first, so the poller is already unblocked when we join it.
        self._stop.take();
        if let Some(thread) = self.thread.take() {
            // Joined rather than detached: the thread owns the cursor until it
            // has cleared its own lines, and a run that returned while a redraw
            // was in flight would otherwise print its result into the display.
            let _ = thread.join();
        }
    }
}

/// `GET /status/<arg_tree>`, or None when the server has nothing to show.
fn fetch(server: &str, arg_tree: &str) -> Result<Option<String>, String> {
    let body = crate::http_get(&format!(
        "{}/status/{arg_tree}",
        server.trim_end_matches('/')
    ))?;
    let text = String::from_utf8_lossy(&body).trim().to_string();
    Ok((text != "null" && !text.is_empty()).then_some(text))
}

/// Flatten the tree into display lines, deepest-last within each parent.
fn render(node: &Node, depth: usize, out: &mut Vec<String>) {
    let now = now_us();
    // `requested` with no `started` is a job waiting for capacity, and saying so
    // is most of why this display is worth having: an idle machine mid-run means
    // something is queued, and that is otherwise invisible until it times out.
    let (state, since) = match (node.started, node.requested) {
        (Some(started), _) => ("run", started),
        (None, Some(requested)) => ("queue", requested),
        (None, None) => ("?", now),
    };
    let secs = now.saturating_sub(since) / 1_000_000;
    // The state/elapsed columns come first and the DEPTH indents the name, so
    // the numbers stay in one column however deep the tree goes — a nested tree
    // whose times step rightwards with it is unreadable as a scan.
    out.push(format!(
        "{state:>5} {secs:>4}s  {:indent$}{}",
        "",
        node.name,
        indent = depth * 2
    ));
    for child in &node.children {
        render(child, depth + 1, out);
    }
}

/// Redraw, replacing the `previous` lines already on screen. Returns how many
/// lines are now drawn.
fn draw(previous: usize, lines: &[String]) -> usize {
    let shown: Vec<&String> = lines.iter().take(MAX_LINES).collect();
    let hidden = lines.len().saturating_sub(shown.len());
    let mut err = std::io::stderr();
    let mut buf = String::new();
    if previous > 0 {
        // Up `previous` lines, then clear everything below the cursor: one
        // escape for the whole display, so a shrinking tree leaves no orphans.
        buf.push_str(&format!("\x1b[{previous}A\x1b[J"));
    }
    for line in &shown {
        buf.push_str(line);
        buf.push('\n');
    }
    let mut drawn = shown.len();
    if hidden > 0 {
        buf.push_str(&format!("      … {hidden} more\n"));
        drawn += 1;
    }
    let _ = err.write_all(buf.as_bytes());
    let _ = err.flush();
    drawn
}

/// Erase the display, leaving the terminal as we found it.
fn clear(drawn: usize) {
    if drawn == 0 {
        return;
    }
    let mut err = std::io::stderr();
    let _ = err.write_all(format!("\x1b[{drawn}A\x1b[J").as_bytes());
    let _ = err.flush();
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, requested: Option<u64>, started: Option<u64>) -> Node {
        Node {
            name: name.to_string(),
            requested,
            started,
            children: Vec::new(),
        }
    }

    #[test]
    fn a_queued_job_is_marked_as_waiting_not_running() {
        let mut lines = Vec::new();
        render(&node("build", Some(now_us()), None), 0, &mut lines);
        assert!(lines[0].contains("queue"), "got: {}", lines[0]);
        assert!(!lines[0].contains("run"), "got: {}", lines[0]);
    }

    #[test]
    fn children_are_indented_under_their_parent_but_the_columns_stay_put() {
        let mut root = node("test", Some(now_us()), Some(now_us()));
        root.children
            .push(node("chat-offline: run one test", Some(now_us()), None));
        let mut lines = Vec::new();
        render(&root, 0, &mut lines);
        assert_eq!(lines.len(), 2);
        // The state column is at the same offset on both lines; only the name
        // moves right.
        assert!(lines[0].starts_with("  run"), "got: {:?}", lines[0]);
        assert!(lines[1].starts_with("queue"), "got: {:?}", lines[1]);
        let name_at = |line: &str| line.find("test").or_else(|| line.find("chat"));
        assert!(
            name_at(&lines[1]) > name_at(&lines[0]),
            "the child's name is indented: {:?} vs {:?}",
            lines[0],
            lines[1]
        );
    }

    #[test]
    fn a_wide_tree_is_capped_with_a_count_of_the_rest() {
        let lines: Vec<String> = (0..MAX_LINES + 5).map(|i| format!("node{i}")).collect();
        // Drawing writes to a stderr that is not a terminal under `cargo test`,
        // so this asserts the arithmetic, which is what can be wrong.
        let drawn = draw(0, &lines);
        assert_eq!(drawn, MAX_LINES + 1, "the capped lines plus the summary");
    }

    #[test]
    fn an_absent_terminal_starts_no_thread() {
        // `cargo test` captures stderr, so this exercises the real branch.
        let watch = Watch::start("http://example.invalid", &"a".repeat(40));
        assert!(watch.thread.is_none());
    }
}
