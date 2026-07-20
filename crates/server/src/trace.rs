//! Bounded, in-memory Chrome traces for compute invocations.
//!
//! Events contain only timing, cache outcome, and unnamed input hashes. They
//! deliberately exclude request shape, argument names, results, and logs.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_TRACES: usize = 100;

#[derive(Clone, Serialize)]
pub(crate) struct Trace {
    #[serde(rename = "traceEvents")]
    events: Vec<Event>,
    #[serde(rename = "displayTimeUnit")]
    display_time_unit: &'static str,
    #[serde(rename = "otherData")]
    metadata: Metadata,
}

#[derive(Clone, Serialize)]
struct Metadata {
    complete: bool,
    event_count: usize,
}

#[derive(Clone, Serialize)]
struct Event {
    name: &'static str,
    ph: &'static str,
    ts: u64,
    dur: u64,
    pid: u32,
    tid: u64,
    args: Args,
}

#[derive(Clone, Serialize)]
struct Args {
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_hit: Option<bool>,
    input_hashes: Vec<String>,
}

struct Span {
    started: Instant,
    started_unix_us: u64,
    input_hashes: Vec<String>,
    cache_hit: Option<bool>,
}

struct Record {
    spans: HashMap<u64, Span>,
    events: Vec<Event>,
    next_span: u64,
    complete: bool,
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
                spans: HashMap::new(),
                events: Vec::new(),
                next_span: 0,
                complete: false,
            },
        );
        Ok(())
    }

    pub(crate) fn start(&self, trace_id: &str) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let record = inner.traces.get_mut(trace_id)?;
        let id = record.next_span;
        record.next_span += 1;
        record.spans.insert(
            id,
            Span {
                started: Instant::now(),
                started_unix_us: unix_us(),
                input_hashes: Vec::new(),
                cache_hit: None,
            },
        );
        Some(id)
    }

    pub(crate) fn inputs(&self, trace_id: &str, span_id: u64, entries: &BTreeMap<String, String>) {
        self.with_span(trace_id, span_id, |span| {
            span.input_hashes = entries.values().cloned().collect();
            span.input_hashes.sort();
        });
    }

    pub(crate) fn cache(&self, trace_id: &str, span_id: u64, hit: bool) {
        self.with_span(trace_id, span_id, |span| span.cache_hit = Some(hit));
    }

    pub(crate) fn finish(&self, trace_id: &str, span_id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = inner.traces.get_mut(trace_id) else {
            return;
        };
        let Some(span) = record.spans.remove(&span_id) else {
            return;
        };
        record.events.push(Event {
            name: "compute",
            ph: "X",
            ts: span.started_unix_us,
            dur: span.started.elapsed().as_micros() as u64,
            pid: std::process::id(),
            tid: span_id,
            args: Args {
                cache_hit: span.cache_hit,
                input_hashes: span.input_hashes,
            },
        });
    }

    pub(crate) fn end(&self, trace_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = inner.traces.get_mut(trace_id) {
            record.complete = true;
        }
    }

    pub(crate) fn get(&self, id: &str, after: usize) -> Option<Trace> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let record = inner.traces.get(id)?;
        Some(Trace {
            events: record.events.iter().skip(after).cloned().collect(),
            display_time_unit: "ms",
            metadata: Metadata {
                complete: record.complete,
                event_count: record.events.len(),
            },
        })
    }

    fn with_span(&self, trace_id: &str, span_id: u64, f: impl FnOnce(&mut Span)) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(span) = inner
            .traces
            .get_mut(trace_id)
            .and_then(|record| record.spans.get_mut(&span_id))
        else {
            return;
        };
        f(span);
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_generic_chrome_events() {
        let store = Store::default();
        store.begin("test").unwrap();
        let span = store.start("test").unwrap();
        let entries = BTreeMap::from([
            ("first".to_string(), "aaa".to_string()),
            ("second".to_string(), "bbb".to_string()),
        ]);
        store.inputs("test", span, &entries);
        store.cache("test", span, false);
        store.finish("test", span);

        store.end("test");
        let value = serde_json::to_value(store.get("test", 0).unwrap()).unwrap();
        let events = value["traceEvents"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "compute");
        assert_eq!(events[0]["ph"], "X");
        for field in ["ts", "dur", "pid", "tid"] {
            assert!(events[0][field].is_number());
        }
        assert_eq!(events[0]["args"]["cache_hit"], false);
        assert_eq!(
            events[0]["args"]["input_hashes"],
            serde_json::json!(["aaa", "bbb"])
        );
        assert!(events[0]["args"].get("first").is_none());
        assert!(events[0].get("result").is_none());
        assert_eq!(value["otherData"]["complete"], true);
        assert_eq!(value["otherData"]["event_count"], 1);
        assert!(store.get("test", 1).unwrap().events.is_empty());
    }
}
