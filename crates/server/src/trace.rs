//! Bounded, in-memory Chrome traces for compute invocations.
//!
//! Events contain only timing, cache outcome, and unnamed input hashes. They
//! deliberately exclude request shape, argument names, results, and logs.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};
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
    started: bool,
    complete: bool,
}

#[derive(Default)]
struct Inner {
    traces: HashMap<String, Record>,
    order: VecDeque<String>,
}

#[derive(Default)]
struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

#[derive(Clone, Default)]
pub(crate) struct Store {
    shared: Arc<Shared>,
}

pub(crate) struct Stream {
    shared: Arc<Shared>,
    trace_id: String,
    cursor: usize,
    pending: Vec<u8>,
    offset: usize,
    done: bool,
}

impl Store {
    pub(crate) fn begin(&self, id: &str) -> Result<(), String> {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = inner.traces.get_mut(id) {
            if record.started {
                return Err(format!("trace {id:?} already exists"));
            }
            record.started = true;
            return Ok(());
        }
        insert_record(&mut inner, id, true);
        Ok(())
    }

    pub(crate) fn start(&self, trace_id: &str) -> Option<u64> {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
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
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
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
        drop(inner);
        self.shared.changed.notify_all();
    }

    pub(crate) fn end(&self, trace_id: &str) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(record) = inner.traces.get_mut(trace_id) {
            record.complete = true;
        }
        drop(inner);
        self.shared.changed.notify_all();
    }

    pub(crate) fn get(&self, id: &str, after: usize) -> Option<Trace> {
        let inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
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

    pub(crate) fn stream(&self, id: &str, after: usize) -> Stream {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !inner.traces.contains_key(id) {
            insert_record(&mut inner, id, false);
        }
        Stream {
            shared: Arc::clone(&self.shared),
            trace_id: id.to_string(),
            cursor: after,
            pending: Vec::new(),
            offset: 0,
            done: false,
        }
    }

    fn with_span(&self, trace_id: &str, span_id: u64, f: impl FnOnce(&mut Span)) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
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

fn insert_record(inner: &mut Inner, id: &str, started: bool) {
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
            started,
            complete: false,
        },
    );
}

impl Stream {
    fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        let mut inner = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let Some(record) = inner.traces.get(&self.trace_id) else {
                self.done = true;
                return Ok(None);
            };
            if let Some(event) = record.events.get(self.cursor) {
                let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
                line.push(b'\n');
                self.cursor += 1;
                return Ok(Some(line));
            }
            if record.complete {
                self.done = true;
                return Ok(Some(b"{\"complete\":true}\n".to_vec()));
            }
            inner = self
                .shared
                .changed
                .wait(inner)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len() {
            let Some(line) = self.next_line()? else {
                return Ok(0);
            };
            self.pending = line;
            self.offset = 0;
        }
        let len = buf.len().min(self.pending.len() - self.offset);
        buf[..len].copy_from_slice(&self.pending[self.offset..self.offset + len]);
        self.offset += len;
        Ok(len)
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

    #[test]
    fn streams_events_from_a_reserved_trace() {
        let store = Store::default();
        let mut stream = store.stream("test", 0);
        let reader = std::thread::spawn(move || {
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            body
        });

        store.begin("test").unwrap();
        let span = store.start("test").unwrap();
        store.cache("test", span, true);
        store.finish("test", span);
        store.end("test");

        let body = reader.join().unwrap();
        let mut lines = body.lines();
        let event: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(event["name"], "compute");
        assert_eq!(event["args"]["cache_hit"], true);
        assert_eq!(lines.next(), Some("{\"complete\":true}"));
        assert_eq!(lines.next(), None);
    }
}
