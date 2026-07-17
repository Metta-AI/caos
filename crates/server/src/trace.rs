//! Bounded, in-memory invocation traces for local observability.
//!
//! Traces intentionally contain only content hashes, topology, timing, and
//! cache state. Raw arguments and worker logs never enter this store.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_TRACES: usize = 100;

#[derive(Clone, Serialize)]
pub(crate) struct Span {
    pub(crate) id: u64,
    pub(crate) parent_id: Option<u64>,
    pub(crate) edge: String,
    pub(crate) req: String,
    pub(crate) image: Option<String>,
    pub(crate) args: Option<String>,
    pub(crate) arg_entries: BTreeMap<String, String>,
    pub(crate) cache_hit: Option<bool>,
    pub(crate) started_ms: u64,
    pub(crate) elapsed_ms: Option<u64>,
    pub(crate) result: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) started_unix_ms: u64,
    pub(crate) elapsed_ms: Option<u64>,
    pub(crate) result: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) spans: Vec<Span>,
}

struct Record {
    snapshot: Snapshot,
    started: Instant,
    span_started: HashMap<u64, Instant>,
    next_span: u64,
}

#[derive(Default)]
struct Inner {
    traces: HashMap<String, Record>,
    order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct Store {
    inner: Mutex<Inner>,
}

impl Store {
    pub(crate) fn begin(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.traces.contains_key(id) {
            return Err(format!("trace {id:?} already exists"));
        }
        while inner.order.len() >= MAX_TRACES {
            if let Some(oldest) = inner.order.pop_front() {
                inner.traces.remove(&oldest);
            }
        }
        inner.order.push_back(id.to_string());
        inner.traces.insert(
            id.to_string(),
            Record {
                snapshot: Snapshot {
                    id: id.to_string(),
                    status: "running".to_string(),
                    started_unix_ms: unix_ms(),
                    elapsed_ms: None,
                    result: None,
                    error: None,
                    spans: Vec::new(),
                },
                started: Instant::now(),
                span_started: HashMap::new(),
                next_span: 0,
            },
        );
        Ok(())
    }

    pub(crate) fn start_span(
        &self,
        trace_id: &str,
        parent_id: Option<u64>,
        edge: &str,
        req: &str,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let record = inner.traces.get_mut(trace_id)?;
        let id = record.next_span;
        record.next_span += 1;
        let now = Instant::now();
        record.span_started.insert(id, now);
        record.snapshot.spans.push(Span {
            id,
            parent_id,
            edge: edge.to_string(),
            req: req.to_string(),
            image: None,
            args: None,
            arg_entries: BTreeMap::new(),
            cache_hit: None,
            started_ms: now.duration_since(record.started).as_millis() as u64,
            elapsed_ms: None,
            result: None,
            error: None,
        });
        Some(id)
    }

    pub(crate) fn request(
        &self,
        trace_id: &str,
        span_id: u64,
        image: &str,
        args: &str,
        arg_entries: &BTreeMap<String, String>,
    ) {
        self.with_span(trace_id, span_id, |span| {
            span.image = Some(image.to_string());
            span.args = Some(args.to_string());
            span.arg_entries = arg_entries.clone();
        });
    }

    pub(crate) fn cache(&self, trace_id: &str, span_id: u64, hit: bool) {
        self.with_span(trace_id, span_id, |span| span.cache_hit = Some(hit));
    }

    pub(crate) fn finish_span(
        &self,
        trace_id: &str,
        span_id: u64,
        result: Option<&str>,
        failed: bool,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = inner.traces.get_mut(trace_id) else {
            return;
        };
        let elapsed = record
            .span_started
            .remove(&span_id)
            .map(|start| start.elapsed().as_millis() as u64);
        if let Some(span) = record.snapshot.spans.iter_mut().find(|s| s.id == span_id) {
            span.elapsed_ms = elapsed;
            span.result = result.map(str::to_string);
            if failed {
                span.error = Some("run failed".to_string());
            }
        }
    }

    pub(crate) fn finish(&self, trace_id: &str, result: Option<&str>, failed: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = inner.traces.get_mut(trace_id) else {
            return;
        };
        record.snapshot.status = if failed { "failed" } else { "completed" }.to_string();
        record.snapshot.elapsed_ms = Some(record.started.elapsed().as_millis() as u64);
        record.snapshot.result = result.map(str::to_string);
        if failed {
            record.snapshot.error = Some("run failed".to_string());
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<Snapshot> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.traces.get(id).map(|r| r.snapshot.clone())
    }

    fn with_span(&self, trace_id: &str, span_id: u64, f: impl FnOnce(&mut Span)) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = inner.traces.get_mut(trace_id) else {
            return;
        };
        if let Some(span) = record.snapshot.spans.iter_mut().find(|s| s.id == span_id) {
            f(span);
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_complete_trace() {
        let store = Store::default();
        store.begin("test").unwrap();
        let span = store.start_span("test", None, "root", "abc").unwrap();
        let entries = std::collections::BTreeMap::from([
            ("bin".to_string(), "bin-hash".to_string()),
            ("image".to_string(), "image".to_string()),
        ]);
        store.request("test", span, "image", "args", &entries);
        store.cache("test", span, false);
        store.finish_span("test", span, Some("blob result"), false);
        store.finish("test", Some("blob result"), false);
        let trace = store.get("test").unwrap();
        assert_eq!(trace.status, "completed");
        assert_eq!(trace.spans.len(), 1);
        assert_eq!(trace.spans[0].cache_hit, Some(false));
        assert_eq!(
            trace.spans[0].arg_entries.get("bin").map(String::as_str),
            Some("bin-hash")
        );
    }
}
