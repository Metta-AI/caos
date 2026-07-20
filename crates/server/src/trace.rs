//! Live Chrome `B`/`E` events with timing, cache outcome, and unnamed input
//! hashes. Events pass directly to the HTTP stream and are not retained.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Serialize)]
struct Event {
    name: &'static str,
    ph: &'static str,
    ts: u64,
    pid: u32,
    tid: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Args>,
}

#[derive(Serialize)]
struct Args {
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_hit: Option<bool>,
    input_hashes: Vec<String>,
}

struct Span {
    input_hashes: Vec<String>,
    cache_hit: Option<bool>,
}

struct Record {
    token: u64,
    sender: mpsc::SyncSender<Event>,
    spans: HashMap<u64, Span>,
    next_span: u64,
    started: bool,
}

#[derive(Default)]
struct Inner {
    traces: HashMap<String, Record>,
    next_token: u64,
}

#[derive(Clone)]
pub(crate) struct Hub {
    inner: Arc<Mutex<Inner>>,
    unix_us: u64,
    monotonic: Instant,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            unix_us: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            monotonic: Instant::now(),
        }
    }
}

pub(crate) struct Stream {
    inner: Arc<Mutex<Inner>>,
    trace_id: String,
    token: u64,
    receiver: mpsc::Receiver<Event>,
}

impl Hub {
    fn now_us(&self) -> u64 {
        self.unix_us
            .saturating_add(self.monotonic.elapsed().as_micros() as u64)
    }

    pub(crate) fn stream(&self, id: &str) -> Result<Stream, String> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.traces.contains_key(id) {
            return Err(format!("trace {id:?} already exists"));
        }
        let token = inner.next_token;
        inner.next_token = inner.next_token.wrapping_add(1);
        let (sender, receiver) = mpsc::sync_channel(0);
        inner.traces.insert(
            id.to_string(),
            Record {
                token,
                sender,
                spans: HashMap::new(),
                next_span: 0,
                started: false,
            },
        );
        Ok(Stream {
            inner: Arc::clone(&self.inner),
            trace_id: id.to_string(),
            token,
            receiver,
        })
    }

    pub(crate) fn begin(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let record = inner
            .traces
            .get_mut(id)
            .ok_or_else(|| format!("trace stream {id:?} is not open"))?;
        if record.started {
            return Err(format!("trace {id:?} already started"));
        }
        record.started = true;
        Ok(())
    }

    pub(crate) fn start(&self, trace_id: &str) -> Option<u64> {
        let ts = self.now_us();
        let (sender, id) = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let record = inner.traces.get_mut(trace_id)?;
            let id = record.next_span;
            record.next_span += 1;
            record.spans.insert(
                id,
                Span {
                    input_hashes: Vec::new(),
                    cache_hit: None,
                },
            );
            (record.sender.clone(), id)
        };
        let _ = sender.send(Event {
            name: "compute",
            ph: "B",
            ts,
            pid: std::process::id(),
            tid: id,
            args: None,
        });
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
        let ts = self.now_us();
        let event = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let Some(record) = inner.traces.get_mut(trace_id) else {
                return;
            };
            let Some(span) = record.spans.remove(&span_id) else {
                return;
            };
            let event = Event {
                name: "compute",
                ph: "E",
                ts,
                pid: std::process::id(),
                tid: span_id,
                args: Some(Args {
                    cache_hit: span.cache_hit,
                    input_hashes: span.input_hashes,
                }),
            };
            (record.sender.clone(), event)
        };
        let _ = event.0.send(event.1);
    }

    pub(crate) fn end(&self, trace_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .traces
            .remove(trace_id);
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

impl Stream {
    fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        match self.receiver.recv() {
            Ok(event) => {
                let mut line = serde_json::to_vec(&event).map_err(std::io::Error::other)?;
                line.push(b'\n');
                Ok(Some(line))
            }
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn respond(mut self, request: tiny_http::Request) -> std::io::Result<()> {
        // tiny-http buffers streaming bodies, so frame and flush each chunk here.
        let version = request.http_version().clone();
        let chunked = version > tiny_http::HTTPVersion(1, 0);
        let mut output = request.into_writer();
        write!(
            output,
            "HTTP/{version} 200 OK\r\nContent-Type: application/x-ndjson\r\n"
        )?;
        if chunked {
            output.write_all(b"Transfer-Encoding: chunked\r\n")?;
        } else {
            output.write_all(b"Connection: close\r\n")?;
        }
        output.write_all(b"\r\n")?;
        output.flush()?;

        while let Some(line) = self.next_line()? {
            if chunked {
                write!(output, "{:x}\r\n", line.len())?;
            }
            output.write_all(&line)?;
            if chunked {
                output.write_all(b"\r\n")?;
            }
            output.flush()?;
        }
        if chunked {
            output.write_all(b"0\r\n\r\n")?;
            output.flush()?;
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner
            .traces
            .get(&self.trace_id)
            .is_some_and(|record| record.token == self.token)
        {
            inner.traces.remove(&self.trace_id);
        }
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn streams_generic_chrome_events_without_retaining_them() {
        let hub = Hub::default();
        let mut stream = hub.stream("test").unwrap();
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            while let Some(line) = stream.next_line().unwrap() {
                lines.push(line);
            }
            lines
        });

        hub.begin("test").unwrap();
        let span = hub.start("test").unwrap();
        let entries = BTreeMap::from([
            ("first".to_string(), "aaa".to_string()),
            ("second".to_string(), "bbb".to_string()),
        ]);
        hub.inputs("test", span, &entries);
        hub.cache("test", span, false);
        hub.finish("test", span);
        hub.end("test");

        let lines = reader.join().unwrap();
        let start: serde_json::Value = serde_json::from_slice(&lines[0]).unwrap();
        assert_eq!(start["name"], "compute");
        assert_eq!(start["ph"], "B");
        for field in ["ts", "pid", "tid"] {
            assert!(start[field].is_number());
        }
        assert!(start.get("dur").is_none());
        assert!(start.get("args").is_none());

        let end: serde_json::Value = serde_json::from_slice(&lines[1]).unwrap();
        assert_eq!(end["name"], "compute");
        assert_eq!(end["ph"], "E");
        assert_eq!(end["pid"], start["pid"]);
        assert_eq!(end["tid"], start["tid"]);
        assert!(end["ts"].as_u64().unwrap() >= start["ts"].as_u64().unwrap());
        assert!(end.get("dur").is_none());
        assert_eq!(end["args"]["cache_hit"], false);
        assert_eq!(
            end["args"]["input_hashes"],
            serde_json::json!(["aaa", "bbb"])
        );
        assert!(end["args"].get("first").is_none());
        assert!(end.get("result").is_none());
        assert_eq!(lines.len(), 2);

        assert!(hub.begin("test").is_err());
        assert!(hub.stream("test").is_ok());
    }

    #[test]
    fn dropping_a_stream_removes_its_reservation() {
        let hub = Hub::default();
        drop(hub.stream("test").unwrap());
        assert!(hub.stream("test").is_ok());
    }

    #[test]
    fn responds_before_the_first_event() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let hub = Hub::default();
        let server_hub = hub.clone();
        let handler = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            server_hub.stream("test").unwrap().respond(request).unwrap();
        });

        let mut response = minreq::get(format!("http://{addr}/trace/test/stream"))
            .with_timeout(2)
            .send_lazy()
            .unwrap();
        assert_eq!(response.status_code, 200);
        let reader = std::thread::spawn(move || {
            let mut body = String::new();
            response.read_to_string(&mut body).unwrap();
            body
        });

        hub.begin("test").unwrap();
        let span = hub.start("test").unwrap();
        hub.finish("test", span);
        hub.end("test");

        let body = reader.join().unwrap();
        assert_eq!(body.lines().count(), 2);
        handler.join().unwrap();
    }
}
