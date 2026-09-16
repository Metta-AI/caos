//! Execution history belongs to commits, never to the content tree.
use super::canonical::{canonical_bytes, parse_canonical};
use super::{records::*, Kind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "event",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Event {
    Request(TurnRecord),
    Tool(CallRecord),
    Async(AsyncRecord),
    Child(ChildRecord),
    Publication(PublicationRecord),
    Payload { path: String, bytes: Vec<u8> },
}

pub fn encode(kind: Kind, events: &[Event]) -> Vec<u8> {
    let mut bytes = kind.message();
    bytes.push(b'\n');
    bytes
        .extend(canonical_bytes(&serde_json::json!({"events": events})).expect("events serialize"));
    bytes
}

pub fn decode(bytes: &[u8]) -> Result<(Kind, Vec<Event>), String> {
    let split = bytes
        .windows(2)
        .position(|s| s == b"\n\n")
        .ok_or("missing conversation events")?;
    let kind = Kind::parse(std::str::from_utf8(&bytes[..split]).map_err(|e| e.to_string())?)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        events: Vec<Event>,
    }
    let body: Body =
        serde_json::from_value(parse_canonical(&bytes[split + 2..])?).map_err(|e| e.to_string())?;
    for event in &body.events {
        match event {
            Event::Request(r) => {
                TurnRecord::from_value(&r.to_value())?;
            }
            Event::Tool(r) => {
                CallRecord::from_value(&r.to_value())?;
            }
            Event::Async(r) => {
                AsyncRecord::from_value(&r.to_value())?;
            }
            Event::Child(r) => {
                ChildRecord::from_value(&r.to_value())?;
            }
            Event::Publication(r) => {
                PublicationRecord::from_value(&r.to_value())?;
            }
            Event::Payload { path, .. } => super::paths::validate_tree_path(path)?,
        }
    }
    Ok((kind, body.events))
}
