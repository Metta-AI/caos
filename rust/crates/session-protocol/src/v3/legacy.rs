//! Carrying a v5 session (one with turn records) into this format.
//!
//! A v5 session cannot be read or forked as it stands: its history holds
//! the turn transitions this format removed, and reading a session walks
//! its history. So a v5 session is carried over instead: a NEW root
//! session is rebuilt from it, with its content, its transcript, and the
//! result of every call it recorded, and a closing system message naming the v5
//! head it came from. The v5 history itself is left untouched, and stays
//! readable by the earlier build.
//!
//! What does not come across is what this format no longer has, and what would
//! no longer validate on a new root: turn records, a call's start and task (the
//! task belongs to the v5 session), a call's source tree and resolution
//! (the pointer move already happened, and is in the content), a message's
//! proposal, and async, subagent and publication records.

use std::collections::BTreeMap;

use serde_json::Value;

use super::apply::{apply, mint, Transition};
use super::ids;
use super::oid::Oid;
use super::paths;
use super::records::{
    Block, CallRecord, Identity, IdentityKind, Role, ToolResult, TranscriptEntry,
};
use super::tree::{Mode, ObjectStore, Signature, Snapshot, TreeBuilder};

const V5_FORMAT: &[u8] = b"caos-conversation-v5\n";

/// Whether `commit` is a v5 session commit.
pub fn is_v5(store: &dyn ObjectStore, commit: &Oid) -> Result<bool, String> {
    let tree = store.read_commit(commit).map_err(String::from)?.tree;
    Ok(Snapshot::new(store, tree).read(paths::FORMAT)?.as_deref() == Some(V5_FORMAT))
}

/// The session id a v5 commit records.
pub fn v5_id(store: &dyn ObjectStore, commit: &Oid) -> Result<String, String> {
    let tree = store.read_commit(commit).map_err(String::from)?.tree;
    let bytes = Snapshot::new(store, tree)
        .read(paths::IDENTITY)?
        .ok_or("v5 session has no identity")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    value["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "v5 session identity has no id".to_string())
}

/// The root commit of the session at `head`, in this format or v5: found by
/// commit kind name alone, which every format shares, so a v5 history with kinds
/// this build no longer has can still be walked.
pub fn session_root(store: &dyn ObjectStore, head: &Oid) -> Result<Oid, String> {
    let mut current = head.clone();
    loop {
        let info = store.read_commit(&current).map_err(String::from)?;
        if kind_name(&info.message) == "conversation.root" {
            return Ok(current);
        }
        current = info
            .parents
            .first()
            .cloned()
            .ok_or_else(|| format!("{current} has no parent before its session's root"))?;
    }
}

/// Rebuild the v5 session at `source` as a new root session `id`,
/// titled `title`, parented on `parent` (G3, or the commit its content came
/// from). Its content is `content` when given (a tree without `.caos/`), or
/// the source's own. Returns the head commit, not pushed.
pub fn migrate_v5(
    store: &mut dyn ObjectStore,
    source: &Oid,
    id: &str,
    title: &str,
    content: Option<Oid>,
    parent: &Oid,
    signature: &Signature,
) -> Result<Oid, String> {
    if !is_v5(store, source)? {
        return Err(format!("{source} is not a v5 session"));
    }
    let source_tree = store.read_commit(source).map_err(String::from)?.tree;
    let content = match content {
        Some(content) => content,
        None => {
            let mut builder = TreeBuilder::from(Some(source_tree.clone()));
            builder.delete(paths::CAOS_DIR);
            builder.build(store)?
        }
    };
    let (tools, payloads) = v5_calls(store, source)?;
    let entries = v5_transcript(store, &source_tree)?;

    let root = Transition::SessionRoot {
        identity: Identity {
            id: id.to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: title.to_string(),
        content: Some(content),
    };
    let applied = apply(store, None, &root)?;
    let mut head = mint(store, parent, &applied, root.kind(), signature)?;

    for (ordinal, (entry, entry_payloads)) in entries.into_iter().enumerate() {
        let ordinal = ordinal as u64;
        let entry = retarget(entry, ordinal);
        let declared: Vec<String> = entry.tool_uses().map(|(id, _, _)| id.to_string()).collect();
        let message_id = entry.message_id.clone();
        append(
            store,
            &mut head,
            Transition::MessageAppend {
                entry,
                payloads: entry_payloads,
            },
            signature,
        )?;
        for call in declared {
            let Some(transition) = carried_call(&tools, &payloads, &call, &message_id)? else {
                continue;
            };
            append(store, &mut head, transition, signature)?;
        }
    }

    let note = ids::protocol_id(
        "v5-migration",
        &serde_json::json!({"source": source.as_str()}),
    )?;
    append(
        store,
        &mut head,
        Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: note,
                session: id.to_string(),
                role: Role::System,
                actor: "caos".to_string(),
                model: None,
                blocks: vec![Block::Text {
                    text: format!(
                        "Carried over from the v5 session at {source}: its content, \
                         transcript and call results, without its turn records."
                    ),
                }],
                proposal: None,
                source_tree_resolution: None,
            },
            payloads: Vec::new(),
        },
        signature,
    )?;
    Ok(head)
}

/// Apply `transition` on `head` and move `head` to the commit it mints.
fn append(
    store: &mut dyn ObjectStore,
    head: &mut Oid,
    transition: Transition,
    signature: &Signature,
) -> Result<(), String> {
    let applied = apply(store, Some(&*head), &transition)?;
    *head = mint(store, &*head, &applied, transition.kind(), signature)?;
    Ok(())
}

/// The kind on a commit message's first line, in any format.
fn kind_name(message: &[u8]) -> &str {
    let line = message.split(|byte| *byte == b'\n').next().unwrap_or(&[]);
    std::str::from_utf8(line).unwrap_or("")
}

type Payloads = BTreeMap<String, Vec<u8>>;

/// A transcript entry and its payloads, by name.
type Entry = (TranscriptEntry, Vec<(String, Vec<u8>)>);

/// The newest record of every call the v5 history holds, by call id, and every
/// payload by path: from the source back to its root, and through a fork into
/// what it forked, as a v5 reader did.
fn v5_calls(
    store: &dyn ObjectStore,
    source: &Oid,
) -> Result<(BTreeMap<String, Value>, Payloads), String> {
    let mut tools = BTreeMap::new();
    let mut payloads = BTreeMap::new();
    let mut current = Some(source.clone());
    while let Some(commit) = current {
        let info = store.read_commit(&commit).map_err(String::from)?;
        let kind = kind_name(&info.message).to_string();
        if let Some(split) = info.message.windows(2).position(|pair| pair == b"\n\n") {
            let body: Value = serde_json::from_slice(&info.message[split + 2..])
                .map_err(|error| format!("reading the events of {commit}: {error}"))?;
            let events = body["events"].as_array().cloned().unwrap_or_default();
            for event in events.into_iter().rev() {
                match event["event"].as_str() {
                    Some("tool") => {
                        if let Some(id) = event["value"]["id"].as_str() {
                            tools
                                .entry(id.to_string())
                                .or_insert_with(|| event["value"].clone());
                        }
                    }
                    Some("payload") => {
                        let path = event["value"]["path"].as_str();
                        let bytes: Option<Vec<u8>> =
                            serde_json::from_value(event["value"]["bytes"].clone()).ok();
                        if let (Some(path), Some(bytes)) = (path, bytes) {
                            payloads.entry(path.to_string()).or_insert(bytes);
                        }
                    }
                    _ => {}
                }
            }
        }
        if kind == "conversation.root" {
            break;
        }
        current = info.parents.first().cloned();
    }
    Ok((tools, payloads))
}

/// The v5 transcript, in order: each entry without the fields this format
/// dropped or a new root could not validate, with its payloads.
fn v5_transcript(store: &dyn ObjectStore, tree: &Oid) -> Result<Vec<Entry>, String> {
    let snapshot = Snapshot::new(store, tree.clone());
    if snapshot.entry(paths::TRANSCRIPT_DIR)?.is_none() {
        return Ok(Vec::new());
    }
    let mut files = BTreeMap::new();
    for entry in snapshot.list(paths::TRANSCRIPT_DIR)? {
        if entry.mode == Mode::Tree {
            continue;
        }
        let path = format!("{}/{}", paths::TRANSCRIPT_DIR, entry.name);
        let (ordinal, message_id) = paths::parse_transcript_entry_path(&path)?;
        files.insert(ordinal, (message_id, path));
    }
    let mut entries = Vec::new();
    for (ordinal, (message_id, path)) in files {
        let bytes = snapshot
            .read(&path)?
            .ok_or_else(|| format!("v5 transcript entry {path} is absent"))?;
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if let Some(object) = value.as_object_mut() {
            for dropped in [
                "request",
                "round",
                "proposal",
                "source_tree_resolution",
                "workspace_resolution",
            ] {
                object.remove(dropped);
            }
        }
        let entry = TranscriptEntry::from_value(&value)
            .map_err(|error| format!("v5 transcript entry {path}: {error}"))?;
        let dir = paths::transcript_payload_dir(ordinal, &message_id);
        let mut payloads = Vec::new();
        if snapshot.entry(&dir)?.is_some() {
            for file in snapshot.list(&dir)? {
                if file.mode == Mode::Tree {
                    continue;
                }
                let bytes = snapshot
                    .read(&format!("{dir}/{}", file.name))?
                    .ok_or_else(|| format!("v5 payload {dir}/{} is absent", file.name))?;
                payloads.push((file.name, bytes));
            }
        }
        entries.push((entry, payloads));
    }
    Ok(entries)
}

/// An entry's payload references moved to the payload directory of the
/// ordinal it lands at, which differs from its v5 one only past a gap.
fn retarget(mut entry: TranscriptEntry, ordinal: u64) -> TranscriptEntry {
    let dir = paths::transcript_payload_dir(ordinal, &entry.message_id);
    for block in &mut entry.blocks {
        let target = match block {
            Block::Payload { path } => path,
            Block::ToolUse { arguments, .. } => arguments,
            Block::Text { .. } => continue,
        };
        if let Some(name) = target.rsplit('/').next() {
            *target = format!("{dir}/{name}");
        }
    }
    entry
}

/// The finished v5 call `id`, as a startless completion declared by `message`,
/// with its payloads moved under its id. `None` for a call that never finished,
/// or whose result payload the history does not hold: it stays unfinished.
fn carried_call(
    tools: &BTreeMap<String, Value>,
    payloads: &Payloads,
    id: &str,
    message: &str,
) -> Result<Option<Transition>, String> {
    let Some(record) = tools.get(id) else {
        return Ok(None);
    };
    if record["status"]
        .as_str()
        .is_none_or(|status| status == "started")
    {
        return Ok(None);
    }
    let Ok(result) = ToolResult::from_value(&record["result"]) else {
        return Ok(None);
    };
    let new_dir = paths::call_payload_dir(id);
    let moved = |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        format!("{new_dir}/{name}")
    };
    // The v5 directory is the one the result's payload sits in.
    let old_path = match &result {
        ToolResult::Complete { observation, .. } => Some(observation.clone()),
        ToolResult::Failed { error } => Some(error.clone()),
        ToolResult::Cancelled { .. } => None,
    };
    let mut carried = Vec::new();
    if let Some(old_path) = &old_path {
        let Some(bytes) = payloads.get(old_path) else {
            return Ok(None);
        };
        let name = old_path.rsplit('/').next().unwrap_or(old_path).to_string();
        carried.push((name, bytes.clone()));
    }
    let result = match result {
        ToolResult::Complete { observation, .. } => ToolResult::Complete {
            observation: moved(&observation),
            proposal: None,
        },
        ToolResult::Failed { error } => ToolResult::Failed {
            error: moved(&error),
        },
        cancelled => cancelled,
    };
    let name = record["name"].as_str().unwrap_or("tool").to_string();
    Ok(Some(Transition::ToolComplete {
        record: CallRecord {
            id: id.to_string(),
            name,
            declaration_message: message.to_string(),
            source_tree_name: None,
            input_commit: None,
            status: CallRecord::expected_status(&result, None),
            task: None,
            result: Some(result),
            source_tree_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        },
        payloads: carried,
        files: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::v3::apply::client_signature;
    use crate::v3::oid::ensure_genesis;
    use crate::v3::tree::{CommitInfo, MemoryStore};
    use crate::v3::validate_spine;
    use crate::v3::view::Session;

    const REQUEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A v5 commit as the earlier build wrote one: `kind`, then its events.
    fn v5_commit(
        store: &mut MemoryStore,
        parent: &Oid,
        tree: &Oid,
        kind: &str,
        events: Value,
    ) -> Oid {
        let signature = client_signature("Old", "old@example.com", 1_700_000_000);
        let mut message = format!("{kind}\n\n").into_bytes();
        message.extend(serde_json::to_vec(&serde_json::json!({ "events": events })).unwrap());
        message.push(b'\n');
        store
            .write_commit(&CommitInfo {
                tree: tree.clone(),
                parents: vec![parent.clone()],
                author: signature.clone(),
                committer: signature,
                extra_headers: Vec::new(),
                message,
            })
            .unwrap()
    }

    fn json(value: Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    /// A v5 session: a prompt, an admitted request, an answer declaring a
    /// read, the read's completion, and the request's terminal.
    fn v5_session(store: &mut MemoryStore) -> Oid {
        let genesis = ensure_genesis(store).unwrap();
        let mut tree = TreeBuilder::from(None);
        tree.put(paths::FORMAT, Mode::Blob, V5_FORMAT.to_vec());
        tree.put(
            paths::IDENTITY,
            Mode::Blob,
            json(serde_json::json!({"id": "old"})),
        );
        tree.put(paths::TITLE, Mode::Blob, b"Old\n".to_vec());
        tree.put("notes.md", Mode::Blob, b"kept\n".to_vec());
        let root_tree = tree.build(store).unwrap();
        let root = v5_commit(
            store,
            &genesis,
            &root_tree,
            "conversation.root",
            Value::Array(vec![]),
        );

        let mut tree = TreeBuilder::from(Some(root_tree));
        tree.put(
            &paths::transcript_entry_path(0, "u1"),
            Mode::Blob,
            json(serde_json::json!({
                "message_id": "u1", "conversation": "old", "role": "user", "actor": "me",
                "blocks": [{"type": "text", "text": "read the notes"}],
            })),
        );
        let prompted_tree = tree.build(store).unwrap();
        let prompted = v5_commit(
            store,
            &root,
            &prompted_tree,
            "message.append",
            Value::Array(vec![]),
        );
        let admitted = v5_commit(
            store,
            &prompted,
            &prompted_tree,
            "request.admit",
            serde_json::json!([{"event": "request", "value": {"id": REQUEST, "round": 0}}]),
        );

        let dir = paths::transcript_payload_dir(1, "a1");
        let mut tree = TreeBuilder::from(Some(prompted_tree));
        tree.put(
            &paths::transcript_entry_path(1, "a1"),
            Mode::Blob,
            json(serde_json::json!({
                "message_id": "a1", "conversation": "old", "role": "assistant",
                "actor": "model", "request": REQUEST, "round": 0, "model": "model",
                "blocks": [{"type": "tool_use", "id": "call-1", "name": "read",
                            "arguments": {"path": format!("{dir}/args-call-1.json")}}],
            })),
        );
        tree.put(
            &format!("{dir}/args-call-1.json"),
            Mode::Blob,
            b"{}".to_vec(),
        );
        tree.put(&format!("{dir}/response.json"), Mode::Blob, b"[]".to_vec());
        let answered_tree = tree.build(store).unwrap();
        let answered = v5_commit(
            store,
            &admitted,
            &answered_tree,
            "model.complete",
            Value::Array(vec![]),
        );

        let observation = format!(".caos/tools/{REQUEST}/0000/call-1/observation.json");
        let completed = v5_commit(
            store,
            &answered,
            &answered_tree,
            "tool.complete",
            serde_json::json!([
                {"event": "payload", "value": {"path": observation, "bytes": b"\"kept\"".to_vec()}},
                {"event": "tool", "value": {
                    "request": REQUEST, "round": 0, "id": "call-1", "name": "read",
                    "declaration_message": "a1", "source_tree_name": null, "input_commit": null,
                    "status": "complete",
                    "result": {"kind": "complete", "observation": observation},
                    "files": [], "files_outcome": null,
                }},
            ]),
        );
        v5_commit(
            store,
            &completed,
            &answered_tree,
            "request.terminal",
            serde_json::json!([{"event": "request", "value": {"id": REQUEST}}]),
        )
    }

    #[test]
    fn a_v5_session_is_carried_into_a_new_root() {
        let mut store = MemoryStore::new();
        let source = v5_session(&mut store);
        assert!(is_v5(&store, &source).unwrap());
        assert_eq!(v5_id(&store, &source).unwrap(), "old");
        // This build cannot read it as it stands, and says why.
        let refused = Session::open(&store, &source).err().unwrap();
        assert!(refused.contains("v5"), "{refused}");

        let genesis = ensure_genesis(&mut store).unwrap();
        let signature = client_signature("New", "new@example.com", 1_700_000_100);
        let head = migrate_v5(
            &mut store, &source, "new", "Carried", None, &genesis, &signature,
        )
        .unwrap();
        validate_spine(&store, &head, &mut HashSet::new()).unwrap();
        assert!(!is_v5(&store, &head).unwrap());

        let view = Session::open(&store, &head).unwrap();
        assert_eq!(view.identity().unwrap().id, "new");
        assert_eq!(view.identity().unwrap().kind, IdentityKind::Root);
        assert_eq!(view.title().unwrap(), "Carried");
        assert_eq!(view.file("notes.md").unwrap(), Some(b"kept\n".to_vec()));
        let transcript = view.transcript(0, view.transcript_len().unwrap()).unwrap();
        let roles: Vec<Role> = transcript.iter().map(|(_, _, entry)| entry.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::System]);
        let tool = view.tool("call-1").unwrap().unwrap();
        assert_eq!(tool.declaration_message, "a1");
        let Some(ToolResult::Complete { observation, .. }) = tool.result else {
            panic!("the read did not come across complete");
        };
        assert_eq!(
            observation,
            format!("{}/observation.json", paths::call_payload_dir("call-1"))
        );
        assert_eq!(view.payload(&observation).unwrap(), b"\"kept\"".to_vec());
        let (_, _, note) = &transcript[2];
        assert!(
            matches!(&note.blocks[..], [Block::Text { text }] if text.contains(source.as_str()))
        );
    }
}
