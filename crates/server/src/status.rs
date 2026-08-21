//! Trace records: what happened to one ArgTree, and `GET /status`.
//!
//! One redis key per ArgTree (`caos:trace:<hash>`), holding an append-only list
//! of typed JSON events (SPEC.md "Tracing"). The ArgTree is also the cache key,
//! which is the point: work runs once, so its record describes that run, and a
//! later invocation that reuses the work reads the record the run left behind.
//!
//! **Nothing here may fail a run.** Every append is best-effort and warns on
//! failure — the reliability principle ("die when unexpected things happen")
//! is about the work, and a lost trace event is not a reason to lose the work.
//!
//! Three things follow from choosing a LIST over a rewritten blob:
//!
//! - Appends need no read-modify-write, so the threads a map fans out onto can
//!   record their children without coordinating.
//! - The record survives a job that never finishes. A `started` with no `ended`
//!   IS the hung-or-killed record — the case you most want to look at, and the
//!   one a write-once-at-completion blob would have nothing to say about.
//! - One key per node means eviction is ATOMIC. This redis runs `allkeys-lru`
//!   at a 2gb cap (`stack/serve`), so a record split across several keys would
//!   be partially evicted — a start with no children, indistinguishable from a
//!   job that hung. You either have the node or you don't.
//!
//! The list is deliberately NOT `LTRIM`ed. Trimming the head loses `requested`
//! and `started` (the node then looks like it never ran); trimming the tail
//! loses `ended` (it looks hung). Both lie. What actually threatened to grow
//! without bound was worker-supplied `/cas/out-trace` data, and that is bounded
//! at the source instead: it is stored as a git object and the event carries
//! only its hash.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Config;

/// One recorded fact about an ArgTree's run. `kind` is the tag; the rest are
/// per-kind and omitted when absent, so a reader can ignore events it does not
/// understand rather than failing to parse the record.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Event {
    /// `requested` | `started` | `ended` | `child` | `continuation` | `out-trace`
    pub(crate) kind: String,
    /// Unix microseconds. WALL clock, not a process-monotonic offset: these are
    /// compared ACROSS runs and across server processes (that is the whole of
    /// SPEC's cache-hit/eviction inference), so a per-process anchor would make
    /// two records incomparable.
    pub(crate) ts: u64,
    /// `ended`: did the work succeed? Never the value, just the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ok: Option<bool>,
    /// `child`: the child's name (a map-then entry name, an eval's expression
    /// path). `continuation`: the promise type (`map`/`run`/`request`/`eval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// `child`: the child's ArgTree. `continuation`: the handler's ArgTree —
    /// the CURRIED one that actually ran, which for a `then` is not derivable
    /// from the continuation (its extra arg is a result).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arg_tree: Option<String>,
    /// `out-trace`: the git object the worker left at `/cas/out-trace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) oid: Option<String>,
    /// `child`: the promise type this child came through.
    ///
    /// Carried on the child rather than read off the `continuation` event
    /// because the continuation is not written until the handler's ArgTree
    /// exists, which is after every child has finished — so an IN-FLIGHT map,
    /// the case the live view is for, would have no type to consult.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) via: Option<String>,
}

impl Event {
    fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            ts: now_us(),
            ok: None,
            name: None,
            arg_tree: None,
            oid: None,
            via: None,
        }
    }
}

/// Unix microseconds now.
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// The redis key holding `arg_tree`'s record.
fn key(arg_tree: &str) -> String {
    format!("caos:trace:{arg_tree}")
}

/// Append one event, best-effort.
fn record(config: &Config, arg_tree: &str, event: Event) {
    let encoded = match serde_json::to_string(&event) {
        Ok(encoded) => encoded,
        Err(err) => {
            eprintln!("warning: cannot encode trace event for {arg_tree}: {err}");
            return;
        }
    };
    if let Err(err) = crate::compute::list_append(&config.redis_addr, &key(arg_tree), &encoded) {
        eprintln!("warning: cannot record trace event for {arg_tree}: {err}");
    }
}

/// The work was admitted and will run.
///
/// Recorded on the path that ACTUALLY RUNS, so a cache hit and a single-flight
/// waiter record nothing — a hit does no work, and its node is the record the
/// original run left. That is what makes the SPEC inference sound: a child whose
/// record ends before its parent started was reused, not re-run.
///
/// Recorded BEFORE dispatch, not when a container appears. A job parked waiting
/// for a runner is the failure that otherwise produces no container and no log
/// line at all, and `started - requested` is the only place that wait is visible.
pub(crate) fn requested(config: &Config, arg_tree: &str) {
    record(config, arg_tree, Event::new("requested"));
}

/// A runner claimed the job and the work is now under way.
pub(crate) fn started(config: &Config, arg_tree: &str) {
    record(config, arg_tree, Event::new("started"));
}

/// The whole request finished — CONTINUATION RESOLUTION INCLUDED.
///
/// Not when the container exited. A promise's children are dispatched after the
/// worker is gone (`compute::resolve_promise`), so an `ended` that stopped at
/// the container would put every map child after its parent's end, and SPEC's
/// fourth rule would read all of them as "evicted and later rerun".
pub(crate) fn ended(config: &Config, arg_tree: &str, ok: bool) {
    let mut event = Event::new("ended");
    event.ok = Some(ok);
    record(config, arg_tree, event);
}

/// This run dispatched `child` under `name`.
///
/// The edge is recorded by the PARENT, which is why a cache-hit child is still
/// reachable: the child writes nothing, but the parent names it, and the child's
/// own (older) record supplies the times.
pub(crate) fn child(config: &Config, arg_tree: &str, via: &str, name: &str, child: &str) {
    let mut event = Event::new("child");
    event.name = Some(name.to_string());
    event.arg_tree = Some(child.to_string());
    event.via = Some(via.to_string());
    record(config, arg_tree, event);
}

/// The promise this run left behind: its type, and the handler ArgTree that ran
/// after the child work (absent when the continuation has no `then`).
pub(crate) fn continuation(config: &Config, arg_tree: &str, kind: &str, handler: Option<&str>) {
    let mut event = Event::new("continuation");
    event.name = Some(kind.to_string());
    event.arg_tree = handler.map(str::to_string);
    record(config, arg_tree, event);
}

/// Perf data the worker chose to leave at `/cas/out-trace`, by hash.
pub(crate) fn out_trace(config: &Config, arg_tree: &str, oid: &str) {
    let mut event = Event::new("out-trace");
    event.oid = Some(oid.to_string());
    record(config, arg_tree, event);
}

/// One ArgTree's record, parsed. Absent, empty and unreadable all come back as
/// an empty record: a trace is a best-effort thing to have, so a reader asks
/// what happened and gets "nothing recorded" rather than an error to handle.
#[derive(Default, Debug)]
pub(crate) struct Record {
    pub(crate) events: Vec<Event>,
}

impl Record {
    /// The events of the CURRENT attempt — everything from the last `requested`.
    ///
    /// A key accumulates attempts: failure and `catch` are never cached and
    /// eviction makes work rerun, so one ArgTree can hold several
    /// `requested`…`ended` sequences. `requested` is what opens one, so it is
    /// the delimiter.
    ///
    /// Scoping the whole slice rather than taking the last event of each kind
    /// independently, because per-kind "last" MIXES attempts: a node that
    /// failed after leaving a promise and was then re-run would report the
    /// previous attempt's continuation handler beside the new attempt's start,
    /// and the walk would follow a handler belonging to a run that is over.
    fn current(&self) -> &[Event] {
        match self.events.iter().rposition(|e| e.kind == "requested") {
            Some(start) => &self.events[start..],
            // No opener at all: not a shape we write, but reading every event is
            // the answer that loses the least.
            None => &self.events,
        }
    }

    /// The last event of `kind` in the current attempt.
    fn last(&self, kind: &str) -> Option<&Event> {
        self.current().iter().rev().find(|e| e.kind == kind)
    }

    pub(crate) fn requested_at(&self) -> Option<u64> {
        self.last("requested").map(|e| e.ts)
    }

    pub(crate) fn started_at(&self) -> Option<u64> {
        self.last("started").map(|e| e.ts)
    }

    /// `(time, ok)` of the last completion, or None while the work is in flight.
    pub(crate) fn ended(&self) -> Option<(u64, bool)> {
        self.last("ended").map(|e| (e.ts, e.ok.unwrap_or(true)))
    }

    /// True once the work has finished. A record with a `started` and no `ended`
    /// is in flight — or died mid-run, which reads the same way and is meant to.
    pub(crate) fn done(&self) -> bool {
        self.ended().is_some()
    }

    /// The children this run dispatched, in dispatch order:
    /// `(promise type, name, ArgTree)`.
    pub(crate) fn children(&self) -> Vec<(String, String, String)> {
        self.current()
            .iter()
            .filter(|e| e.kind == "child")
            .filter_map(|e| {
                Some((
                    e.via.clone().unwrap_or_default(),
                    e.name.clone()?,
                    e.arg_tree.clone()?,
                ))
            })
            .collect()
    }

    /// The continuation handler's ArgTree, if this run left a promise with one.
    ///
    /// The promise's TYPE is recorded alongside it and deliberately has no
    /// accessor: the live view names a node by its `help`, and the type is for
    /// a reader of finished runs, which does not exist yet. It is written now
    /// because a record cannot be backfilled — the run is over.
    pub(crate) fn completion(&self) -> Option<String> {
        self.last("continuation").and_then(|e| e.arg_tree.clone())
    }

    pub(crate) fn out_traces(&self) -> Vec<String> {
        self.current()
            .iter()
            .filter(|e| e.kind == "out-trace")
            .filter_map(|e| e.oid.clone())
            .collect()
    }
}

/// Read one ArgTree's record.
pub(crate) fn read(config: &Config, arg_tree: &str) -> Record {
    let raw = match crate::compute::list_read(&config.redis_addr, &key(arg_tree)) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("warning: cannot read trace record for {arg_tree}: {err}");
            return Record::default();
        }
    };
    // A single unparseable event is dropped rather than failing the record: a
    // record is evidence, and partial evidence beats none.
    let events = raw
        .iter()
        .filter_map(|line| match serde_json::from_str::<Event>(line) {
            Ok(event) => Some(event),
            Err(err) => {
                eprintln!("warning: skipping unparseable trace event for {arg_tree}: {err}");
                None
            }
        })
        .collect();
    Record { events }
}

/// One node of the rendered work tree.
#[derive(Serialize, Debug)]
pub(crate) struct Node {
    /// The identity, so a reader can go straight from a node to the thing that
    /// makes it: its ArgTree is its cache key.
    arg_tree: String,
    name: String,
    /// Admitted at. Present whenever we have a record at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    requested: Option<u64>,
    /// Claimed by a runner at, if it has been. Absent means the job is still
    /// waiting for capacity — `requested` with no `started` is the parked job
    /// that otherwise produces no container and no log line at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<u64>,
    /// Git objects the worker left at `/cas/out-trace`, by hash — `caos get`
    /// one to read it.
    ///
    /// Reachable here only for a node that left a PROMISE: out-trace arrives
    /// with the worker's result, and a node whose work is wholly finished is
    /// skipped by the walk. A promising node is not finished until its
    /// continuation is, so its own perf data is on screen for exactly as long
    /// as its fan-out is running — which is when you would be looking.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    out_trace: Vec<String>,
    /// This node's work was REUSED by the run being read, not performed by it:
    /// its record ended before its parent started (SPEC.md "Tracing", the first
    /// inference rule). Only the completed view says anything here — while work
    /// is live, nothing under it has been reused yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reused: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<Node>,
}

/// Which question `/status` is being asked.
#[derive(Clone, Copy, PartialEq)]
enum View {
    /// What is happening NOW. Finished work is skipped and a promise resolves
    /// to the handler it moved to, so the tree is the live frontier.
    Live,
    /// What happened. Nothing is skipped and a continuation handler hangs off
    /// the node that promised it rather than replacing it, so the shape is the
    /// run's actual structure — which is what you diff against another run.
    Complete,
}

/// How deep the `base` search for a name may go before giving up. A malformed
/// or self-referential image would otherwise walk forever, and naming a node is
/// never worth hanging a status request over.
const NAME_SEARCH_DEPTH: usize = 32;

/// `GET /status/<arg tree hash>[?all=1]` — the work under `arg_tree`, as a JSON
/// tree (SPEC.md "Tracing"). An ArgTree with nothing to show renders as `null`.
///
/// `all=1` asks what HAPPENED rather than what is happening: the completed view
/// keeps finished nodes and marks the ones whose work was reused.
pub(crate) fn serve(
    config: &Config,
    arg_tree: &str,
    query: &str,
) -> Result<Vec<u8>, crate::HttpError> {
    if arg_tree.len() != 40 || !arg_tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(crate::HttpError::new(
            400,
            "status needs a lowercase 40-character ArgTree hash",
        ));
    }
    let view = if query.split('&').any(|p| p == "all=1" || p == "all") {
        View::Complete
    } else {
        View::Live
    };
    let node = walk(config, arg_tree, None, view, None, &mut 0);
    serde_json::to_vec(&node)
        .map_err(|e| crate::HttpError::new(500, format!("encoding status: {e}")))
}

/// Render `arg_tree`'s subtree, or None if it has nothing to show.
///
/// `prefix` qualifies the node's own name, and comes from one of two places:
///
/// - descending into a CHILD, it is the name the parent gave it — a map entry's
///   name (a test name, for the suite) or an eval's expression path;
/// - following a COMPLETION, it is the name of the node we came from, because a
///   continuation handler is the same logical work one stage on.
///
/// It is set once and carried, not accumulated: a five-hop promise chain reads
/// `<the tool>: fanout`, not the whole chain concatenated.
///
/// `parent_started` is when the node ABOVE began, and is what decides whether
/// this one was reused: a record that ended before its parent started belongs to
/// an earlier run (SPEC.md "Tracing"). None at the root, which by definition is
/// the run being read.
///
/// `budget` bounds the whole walk. The tree is read from records that a
/// concurrent run is still appending to, so a malformed or circular set of
/// child edges must not turn a status request into an unbounded traversal.
fn walk(
    config: &Config,
    arg_tree: &str,
    prefix: Option<&str>,
    view: View,
    parent_started: Option<u64>,
    budget: &mut usize,
) -> Option<Node> {
    const MAX_NODES: usize = 10_000;
    if *budget >= MAX_NODES {
        return None;
    }
    *budget += 1;

    let record = read(config, arg_tree);

    // No record at all means this ArgTree has never run here — not that it is
    // pending. Rendering it would invent a node with no times, which reads as
    // work waiting to start; "we know nothing about this key" is the honest
    // answer and it is the same shape as "nothing is current".
    if record.events.is_empty() {
        return None;
    }
    // Finished work with nothing left to point at has nothing left to say —
    // in the LIVE view, where a completed leaf is not current work. The
    // completed view is asking the opposite question and keeps it.
    //
    // The test is "no completion", not "no promise". A continuation with no
    // `then` (a bare `map`, whose children's results ARE the request's result)
    // has a promise and no handler, so a promise test would keep rendering it
    // as a leaf long after everything under it had finished.
    if view == View::Live && record.done() && record.completion().is_none() {
        return None;
    }

    // Reused work is a LEAF here, marked and not descended into. Its children
    // belong to the run that performed it, and following them would silently
    // splice another invocation's tree into this one — which is exactly the
    // confusion the reuse mark exists to prevent.
    let reused = parent_started
        .is_some_and(|started| record.ended().is_some_and(|(ended, _)| ended < started));
    if reused {
        return Some(leaf(config, arg_tree, prefix, &record, true));
    }

    // A promise that produced a handler HAS MOVED to that handler: the
    // continuation's ArgTree is only formed once the middle step is over, so
    // its existence is itself the proof that the children are finished.
    //
    // Gated on the handler existing rather than on `done()`, and that is not a
    // detail. `ended` covers continuation resolution, so a node with a running
    // `then` is not done — gating on `done()` would mean the completion is only
    // ever followed once the whole request is over, by which time the handler
    // is finished too and gets skipped. The `then` stage would be invisible for
    // exactly as long as it was running, which for `run-tool test` is the whole
    // summarising phase.
    //
    // The LIVE view replaces the node with its handler, because only the
    // frontier is interesting. The completed view hangs the handler off it as a
    // child instead: the shape of what happened is what gets diffed, and a
    // chain that ate its own links is not that shape.
    let carried = prefix
        .map(str::to_string)
        .or_else(|| name_of(config, arg_tree));
    if view == View::Live {
        if let Some(completion) = record.completion() {
            // Carry the qualifier we already have, else adopt this node's own
            // name. The root of a tool chain is the only node with `help` —
            // every curry after it rebuilds the ArgTree from `base` plus a
            // chosen few args (`caos-tools/test/worker.sh`), so `help` is a
            // sibling that is dropped at the first hop and can never be found
            // by descending `base`. Handing it down the chain is what keeps the
            // later stages attributable.
            return walk(
                config,
                &completion,
                carried.as_deref(),
                view,
                parent_started,
                budget,
            );
        }
    }

    // Every child is measured against THIS node's start, which is what makes
    // the reuse mark a statement about the run being read rather than about
    // wall-clock age.
    let started = record.started_at();
    let mut children: Vec<Node> = record
        .children()
        .into_iter()
        .filter_map(|(via, name, child)| {
            // A child's own name REPLACES the parent's qualifier: it is a
            // separate piece of work, not a later stage of this one. `run` and
            // `request` are excluded because those names are the child's
            // position in the continuation, not a description of it — and such
            // a child usually names itself (a `run` step is often another
            // tool's ArgTree, `help` and all).
            let label = matches!(via.as_str(), "map" | "eval").then_some(name);
            walk(config, &child, label.as_deref(), view, started, budget)
        })
        .collect();
    if view == View::Complete {
        if let Some(completion) = record.completion() {
            children.extend(walk(
                config,
                &completion,
                carried.as_deref(),
                view,
                started,
                budget,
            ));
        }
    }

    let mut node = leaf(config, arg_tree, prefix, &record, false);
    node.children = children;
    Some(node)
}

/// A node with no children yet, named and timed from its record.
fn leaf(
    config: &Config,
    arg_tree: &str,
    prefix: Option<&str>,
    record: &Record,
    reused: bool,
) -> Node {
    // The prefix alone, when the node has no name of its own. Appending the
    // image to a named map child gives every line of a fan-out the SAME sixty
    // characters of docker ref after its one distinguishing word, which pushes
    // the useful part off the terminal — measured on a live suite run, where
    // all 46 lines read `<test>: docker://caos-registry:5000/caos@sha256:…`.
    let name = match (prefix, name_of(config, arg_tree)) {
        (Some(prefix), Some(own)) => format!("{prefix}: {own}"),
        (Some(prefix), None) => prefix.to_string(),
        (None, Some(own)) => own,
        (None, None) => image_of(config, arg_tree),
    };
    Node {
        arg_tree: arg_tree.to_string(),
        name,
        requested: record.requested_at(),
        started: record.started_at(),
        out_trace: record.out_traces(),
        reused,
        children: Vec::new(),
    }
}

/// The arg a multi-stage worker uses to say which stage it is up to.
///
/// One name, because SPEC.md ("Worker scripts") mandates one. The Rust workers
/// used to spell it `mode`, which also had to carry `worker-cargo`'s CALLER-
/// facing `--mode=all` — so a display reading it could not tell a request from
/// a position. They are now two args, and this reads only the position.
const STAGE_ARG: &str = "stage";

/// What a node calls itself: the first line of its `help`, else the stage it
/// says it is up to — or None when it says neither.
///
/// The two rungs cover different nodes, and between them almost everything. A
/// tool's ArgTree carries `help` and nothing else does: every curry after it
/// rebuilds from `base` plus a chosen few args, so `help` is dropped at the
/// first hop. What those later stages DO carry is the discriminator they switch
/// on, which is exactly the name of what they are doing (`fanout`, `summarize`,
/// `combine`).
///
/// Optional rather than falling back internally, so the caller can tell "this is
/// what the node is" from "this is only what it runs" and decide which is worth
/// a line of terminal.
fn name_of(config: &Config, arg_tree: &str) -> Option<String> {
    describe(config, arg_tree).0
}

/// A short identifier for the image a node runs, for a node that names itself
/// no other way.
///
/// Abbreviated deliberately. The full `docker://caos-registry:5000/caos@sha256:…`
/// is sixty characters, and it is the SAME sixty for every node of a fan-out —
/// so printed in full it is pure noise that displaces the part that differs.
fn image_of(config: &Config, arg_tree: &str) -> String {
    describe(config, arg_tree).1
}

/// What a node is (if it says) and what it runs (a short image id) — the two
/// halves of a display name, kept apart so the caller can choose.
type Description = (Option<String>, String);

/// `(name, short image id)` for an ArgTree.
///
/// Memoized, because both are pure functions of a CONTENT-ADDRESSED hash and so
/// can never go stale. Without this, a live display of a 46-way fan-out re-walks
/// every child's whole base chain twice a second — the same object reads, for
/// the same answer, forever. The map only grows with the number of distinct
/// ArgTrees a server has been asked about (tens of bytes each), which is why it
/// needs no eviction.
fn describe(config: &Config, arg_tree: &str) -> Description {
    static NAMES: OnceLock<Mutex<HashMap<String, Description>>> = OnceLock::new();
    let names = NAMES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = names
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(arg_tree)
    {
        return hit.clone();
    }
    // Computed OUTSIDE the lock: this reads git objects, and holding a global
    // mutex across that would serialize every node of every concurrent poll.
    // Two threads racing the same answer just both compute it and agree.
    let described = describe_uncached(config, arg_tree);
    names
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(arg_tree.to_string(), described.clone());
    described
}

fn describe_uncached(config: &Config, arg_tree: &str) -> Description {
    // The stage is read from the node's OWN args and nowhere else. A `base` is
    // an image, and a stage it happened to carry would name the run that built
    // it rather than this one.
    let stage = arg_line(config, arg_tree, STAGE_ARG);

    // `help` and the image come out of one descent: `help` can sit on a curried
    // layer above the image, and the image is whatever the chain bottoms out at.
    let mut help = None;
    let mut current = arg_tree.to_string();
    for _ in 0..NAME_SEARCH_DEPTH {
        let Ok(entries) = crate::storage::fetch_tree(config, &current) else {
            // Not a tree: a `docker://…@sha256:…` base is a blob whose content
            // is the ref itself.
            let image = match crate::storage::fetch_blob(config, &current) {
                Ok(bytes) => abbreviate_ref(&first_line(&bytes)),
                Err(_) => short_hash(&current),
            };
            return (help.or(stage), image);
        };
        if help.is_none() {
            help = entries
                .iter()
                .find(|e| e.name == "help")
                .and_then(|e| blob_line(config, &e.oid.to_string()));
        }
        let Some(base) = entries.iter().find(|e| e.name == "base") else {
            // A git image bottoms out at a tree with no `base` of its own.
            return (help.or(stage), short_hash(&current));
        };
        current = base.oid.to_string();
    }
    (help.or(stage), short_hash(&current))
}

/// The first line of a named blob arg of `arg_tree`, if it has one.
fn arg_line(config: &Config, arg_tree: &str, name: &str) -> Option<String> {
    let entries = crate::storage::fetch_tree(config, arg_tree).ok()?;
    let entry = entries.iter().find(|e| e.name == name)?;
    blob_line(config, &entry.oid.to_string())
}

/// The first line of a blob, or None when it is unreadable or says nothing.
fn blob_line(config: &Config, oid: &str) -> Option<String> {
    let bytes = crate::storage::fetch_blob(config, oid).ok()?;
    let line = first_line(&bytes);
    (!line.is_empty()).then_some(line)
}

/// `docker://host/repo@sha256:abcd…` → `repo@abcd1234`. Anything that is not
/// that shape is returned as-is: guessing at an unknown format would hide it.
fn abbreviate_ref(reference: &str) -> String {
    let Some((name, digest)) = reference.rsplit_once('@') else {
        return reference.to_string();
    };
    let repo = name.rsplit('/').next().unwrap_or(name);
    let hex = digest.rsplit_once(':').map_or(digest, |(_, hex)| hex);
    format!("{repo}@{}", &hex[..hex.len().min(8)])
}

/// A hash, shortened to the prefix people actually quote at each other.
fn short_hash(hash: &str) -> String {
    hash[..hash.len().min(8)].to_string()
}

/// The first non-empty line of `bytes`, trimmed, as lossy UTF-8.
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, ts: u64) -> Event {
        let mut event = Event::new(kind);
        event.ts = ts;
        event
    }

    #[test]
    fn reads_the_latest_attempt() {
        // One key, two attempts: a failure and then a rerun. The accessors
        // answer for the CURRENT attempt, not the first one on the list.
        let mut failed = event("ended", 20);
        failed.ok = Some(false);
        let mut ok = event("ended", 40);
        ok.ok = Some(true);
        let record = Record {
            events: vec![
                event("requested", 10),
                event("started", 15),
                failed,
                event("requested", 30),
                event("started", 35),
                ok,
            ],
        };
        assert_eq!(record.requested_at(), Some(30));
        assert_eq!(record.started_at(), Some(35));
        assert_eq!(record.ended(), Some((40, true)));
        assert!(record.done());
    }

    #[test]
    fn a_started_without_an_ended_is_still_running() {
        let record = Record {
            events: vec![event("requested", 10), event("started", 15)],
        };
        assert!(!record.done());
        assert_eq!(record.started_at(), Some(15));
    }

    #[test]
    fn children_keep_their_names_and_order() {
        let mut first = event("child", 10);
        first.name = Some("chat-offline".to_string());
        first.arg_tree = Some("a".repeat(40));
        first.via = Some("map".to_string());
        let mut second = event("child", 11);
        second.name = Some("push-closure".to_string());
        second.arg_tree = Some("b".repeat(40));
        second.via = Some("map".to_string());
        let mut cont = event("continuation", 12);
        cont.name = Some("map".to_string());
        cont.arg_tree = Some("c".repeat(40));
        let record = Record {
            events: vec![first, second, cont],
        };
        assert_eq!(
            record.children(),
            vec![
                (
                    "map".to_string(),
                    "chat-offline".to_string(),
                    "a".repeat(40)
                ),
                (
                    "map".to_string(),
                    "push-closure".to_string(),
                    "b".repeat(40)
                ),
            ]
        );
        assert_eq!(record.completion(), Some("c".repeat(40)));
    }

    #[test]
    fn a_rerun_does_not_inherit_the_previous_attempts_promise() {
        // Attempt 1 fanned out and left a handler, then failed. Attempt 2 has
        // only just been admitted. Reading the handler across the boundary
        // would send the walk into a run that is over.
        let mut child = event("child", 11);
        child.name = Some("first".to_string());
        child.arg_tree = Some("a".repeat(40));
        child.via = Some("map".to_string());
        let mut cont = event("continuation", 12);
        cont.name = Some("map".to_string());
        cont.arg_tree = Some("c".repeat(40));
        let mut failed = event("ended", 13);
        failed.ok = Some(false);
        let record = Record {
            events: vec![
                event("requested", 10),
                child,
                cont,
                failed,
                event("requested", 20),
            ],
        };
        assert_eq!(record.requested_at(), Some(20));
        assert!(!record.done(), "the new attempt has not ended");
        assert_eq!(record.completion(), None, "attempt 1's handler is not ours");
        assert!(
            record.children().is_empty(),
            "attempt 1's child is not ours"
        );
    }

    #[test]
    fn a_continuation_without_a_handler_has_no_completion() {
        let mut cont = event("continuation", 10);
        cont.name = Some("run".to_string());
        let record = Record { events: vec![cont] };
        assert_eq!(record.completion(), None);
    }

    #[test]
    fn a_docker_ref_abbreviates_to_the_part_that_differs() {
        assert_eq!(
            abbreviate_ref("docker://caos-registry:5000/caos@sha256:9e70a80fc3561907fd1ef391"),
            "caos@9e70a80f"
        );
        // Short digests are not padded or truncated past their end.
        assert_eq!(abbreviate_ref("repo@sha256:abc"), "repo@abc");
        // Not a digest-pinned ref: returned whole rather than guessed at.
        assert_eq!(abbreviate_ref("docker://busybox"), "docker://busybox");
        assert_eq!(abbreviate_ref(""), "");
    }

    #[test]
    fn an_empty_record_answers_rather_than_failing() {
        let record = Record::default();
        assert!(!record.done());
        assert_eq!(record.requested_at(), None);
        assert!(record.children().is_empty());
    }

    #[test]
    fn events_round_trip_without_their_absent_fields() {
        let mut ended = Event::new("ended");
        ended.ok = Some(false);
        let encoded = serde_json::to_string(&ended).unwrap();
        assert!(!encoded.contains("name"), "absent fields are omitted");
        assert!(!encoded.contains("arg_tree"));
        let decoded: Event = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.kind, "ended");
        assert_eq!(decoded.ok, Some(false));
        assert_eq!(decoded.name, None);
    }
}
