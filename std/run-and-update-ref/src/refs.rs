//! Minimal terminal-event append protocol for the finish stage.
//!
//! Ref coordination uses ordinary Git directly. Event appends are tree-neutral,
//! so a CAS loser simply retries from the latest head with that head's tree.

use std::collections::HashMap;

use conversation_protocol::{
    validate_conversation_ref, ConversationEvent, EventBoundary, ObjectId,
};
use serde_json::{json, Value};
use worker_common::git::Repo;

const MAX_CAS_ATTEMPTS: usize = 32;

pub fn validate_target_ref(refname: &str) -> Result<(), String> {
    validate_conversation_ref(refname)
        .map_err(|_| format!("target-ref is not a conversation head: {refname:?}"))
}

pub fn append_status(refname: &str, task: &str, status: &str, result: &str) -> Result<(), String> {
    validate_target_ref(refname)?;
    validate_hash(task, "task")?;
    validate_hash(result, "result")?;
    if !matches!(status, "complete" | "failed") {
        return Err(format!("invalid async status {status:?}"));
    }
    let base = server_base()?;
    let repo = Repo::new("run-and-update-ref-git")?;
    let mut commits = HashMap::new();

    for _ in 0..MAX_CAS_ATTEMPTS {
        let head = repo
            .read_ref(refname)?
            .ok_or_else(|| format!("target conversation ref {refname} does not exist"))?;
        let remote = fetch_commit_cached(&base, &head, &mut commits)?;
        // A retry after a caught failure can legitimately succeed: caught
        // failures are not cached, so rerunning the same Q may produce the
        // other terminal outcome. Only the same status and result are
        // idempotent. A different outcome must become the latest durable state
        // so F agrees with the result this execution of Q will return and pin.
        let next = TerminalOutcome {
            status: status.to_string(),
            result: result.to_string(),
        };
        if matches!(
            task_state(&base, &head, task, &mut commits)?.as_ref(),
            Some(TaskState::Terminal(current)) if current == &next
        ) {
            return Ok(());
        }

        let message = json!({"async": {"task": task, "status": status, "result": result}});
        let message = serde_json::to_string(&message)
            .map_err(|error| format!("serializing async status event: {error}"))?;
        let commit = store_commit(&base, &remote.tree, &head, &message)?;
        match repo.push_ref(refname, Some(&head), &commit) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let observed = repo.read_ref(refname).map_err(|read_error| {
                    format!(
                        "pushing {refname} failed ({error}); rereading it also failed: {read_error}"
                    )
                })?;
                if observed.as_deref() == Some(commit.as_str()) {
                    return Ok(());
                }
                if observed.as_deref() == Some(head.as_str()) {
                    return Err(error);
                }
            }
        }
    }
    Err(format!(
        "target ref {refname} kept changing after {MAX_CAS_ATTEMPTS} attempts"
    ))
}

#[derive(Debug, Eq, PartialEq)]
struct TerminalOutcome {
    status: String,
    result: String,
}

#[derive(Debug, Eq, PartialEq)]
enum TaskState {
    Pending,
    Terminal(TerminalOutcome),
}

#[derive(Clone)]
struct RemoteCommit {
    tree: String,
    parent: Option<String>,
    message: String,
}

fn fetch_commit(base: &str, hash: &str) -> Result<RemoteCommit, String> {
    let (kind, content) = get_object(base, hash)?;
    if kind != "commit" {
        return Err(format!("object {hash} is a {kind}, not a commit"));
    }
    let text = std::str::from_utf8(&content).map_err(|e| format!("commit is not UTF-8: {e}"))?;
    let (headers, message) = text
        .split_once("\n\n")
        .ok_or("commit has no header/message separator")?;
    let tree = headers
        .lines()
        .find_map(|line| line.strip_prefix("tree "))
        .ok_or("commit has no tree")?
        .to_string();
    validate_hash(&tree, "commit tree")?;
    let parent = headers
        .lines()
        .find_map(|line| line.strip_prefix("parent "))
        .map(str::to_string);
    if let Some(parent) = &parent {
        validate_hash(parent, "commit parent")?;
    }
    Ok(RemoteCommit {
        tree,
        parent,
        message: message.to_string(),
    })
}

fn fetch_commit_cached(
    base: &str,
    hash: &str,
    cache: &mut HashMap<String, RemoteCommit>,
) -> Result<RemoteCommit, String> {
    cached_commit(cache, hash, || fetch_commit(base, hash))
}

fn cached_commit(
    cache: &mut HashMap<String, RemoteCommit>,
    hash: &str,
    fetch: impl FnOnce() -> Result<RemoteCommit, String>,
) -> Result<RemoteCommit, String> {
    if let Some(commit) = cache.get(hash) {
        return Ok(commit.clone());
    }
    let commit = fetch()?;
    cache.insert(hash.to_string(), commit.clone());
    Ok(commit)
}

fn task_state(
    base: &str,
    head: &str,
    task: &str,
    cache: &mut HashMap<String, RemoteCommit>,
) -> Result<Option<TaskState>, String> {
    let mut current = head.to_string();
    let mut newest_state = None;
    loop {
        let hash = current;
        let commit = fetch_commit_cached(base, &hash, cache)?;
        let event = parse_spine_event(&commit.message, &hash)?;
        let parent = required_event_parent(commit.parent.as_deref(), &hash)?;
        let boundary = ConversationEvent::parse(&event)?.boundary(&parent)?;
        if newest_state.is_none() {
            newest_state = event_task_state(&event, task);
        }
        if boundary == EventBoundary::Root {
            return Ok(newest_state);
        }
        current = parent;
    }
}

fn parse_spine_event(message: &str, commit: &str) -> Result<Value, String> {
    let event = serde_json::from_str::<Value>(message.trim())
        .map_err(|error| format!("conversation history commit {commit} is not JSON: {error}"))?;
    ConversationEvent::parse(&event)
        .map_err(|error| format!("invalid conversation event {commit}: {error}"))?;
    Ok(event)
}

fn required_event_parent(parent: Option<&str>, event: &str) -> Result<String, String> {
    parent
        .map(str::to_string)
        .ok_or_else(|| format!("conversation event {event} has no first parent"))
}

/// Read one event defensively. Conversation history is append-only, so a
/// malformed or future async payload must not prevent a valid finish event
/// from being appended after it.
fn event_task_state(event: &Value, wanted: &str) -> Option<TaskState> {
    let state = event.get("async")?;
    let parsed = (|| {
        let task = state
            .get("task")
            .and_then(Value::as_str)
            .ok_or("conversation async event has no string task")?;
        let status = state
            .get("status")
            .and_then(Value::as_str)
            .ok_or("conversation async event has no string status")?;
        validate_hash(task, "async task")?;
        if !matches!(status, "pending" | "complete" | "failed") {
            return Err(format!("invalid async status {status:?}"));
        }
        if status == "pending" {
            return Ok::<_, String>((task, TaskState::Pending));
        }
        let result = state
            .get("result")
            .and_then(Value::as_str)
            .ok_or("terminal conversation async event has no string result")?;
        validate_hash(result, "async result")?;
        Ok((
            task,
            TaskState::Terminal(TerminalOutcome {
                status: status.to_string(),
                result: result.to_string(),
            }),
        ))
    })();
    match parsed {
        Ok((task, state)) if task == wanted => Some(state),
        Ok(_) => None,
        Err(error) => {
            eprintln!("run-and-update-ref: ignoring malformed async event: {error}");
            None
        }
    }
}

fn store_commit(base: &str, tree: &str, parent: &str, message: &str) -> Result<String, String> {
    validate_hash(tree, "commit tree")?;
    validate_hash(parent, "commit parent")?;
    let content = format!(
        "tree {tree}\nparent {parent}\nauthor caos-async <caos@caos> 0 +0000\n\
         committer caos-async <caos@caos> 0 +0000\n\n{message}"
    );
    store_object(base, "commit", content.as_bytes())
}

fn get_object(base: &str, hash: &str) -> Result<(String, Vec<u8>), String> {
    validate_hash(hash, "object")?;
    let url = format!("{base}/object/{hash}");
    let response = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    parse_object(response.as_bytes())
}

fn parse_object(serialized: &[u8]) -> Result<(String, Vec<u8>), String> {
    let nul = serialized
        .iter()
        .position(|b| *b == 0)
        .ok_or("object has no NUL")?;
    let header = std::str::from_utf8(&serialized[..nul])
        .map_err(|e| format!("object header is not UTF-8: {e}"))?;
    let (kind, size) = header.split_once(' ').ok_or("malformed object header")?;
    let size: usize = size
        .parse()
        .map_err(|e| format!("invalid object size: {e}"))?;
    let content = &serialized[nul + 1..];
    if content.len() != size {
        return Err(format!(
            "object size {size} != content length {}",
            content.len()
        ));
    }
    Ok((kind.to_string(), content.to_vec()))
}

fn store_object(base: &str, kind: &str, content: &[u8]) -> Result<String, String> {
    let mut body = format!("{kind} {}\0", content.len()).into_bytes();
    body.extend_from_slice(content);
    let url = format!("{base}/object/");
    let response = minreq::post(&url)
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let hash = response
        .as_str()
        .map_err(|e| format!("POST {url}: {e}"))?
        .trim();
    validate_hash(hash, "stored object")?;
    Ok(hash.to_string())
}

fn server_base() -> Result<String, String> {
    std::env::var("CAOS_SERVER_URL")
        .map(|base| base.trim_end_matches('/').to_string())
        .map_err(|_| "CAOS_SERVER_URL not set".to_string())
}

pub(crate) fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    ObjectId::parse(hash, what).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_hashes_are_canonical_lowercase() {
        assert!(validate_hash(&"a".repeat(40), "test hash").is_ok());
        assert!(validate_hash(&"A".repeat(40), "test hash")
            .unwrap_err()
            .contains("lowercase"));
    }

    #[test]
    fn recognized_root_event_is_rejected() {
        let event = "a".repeat(40);
        assert!(required_event_parent(None, &event)
            .unwrap_err()
            .contains("no first parent"));
    }

    #[test]
    fn event_envelope_is_selected_by_the_ref_namespace() {
        let head = "a".repeat(40);
        assert_eq!(parse_spine_event(r#"{}"#, &head), Ok(json!({})));
        assert!(parse_spine_event(r#"{"status":"idle"}"#, &head).is_ok());
        assert!(parse_spine_event(r#"{"v":2}"#, &head).is_err());
        assert!(parse_spine_event(r#"[]"#, &head).is_err());
        assert!(parse_spine_event("ordinary commit", &head).is_err());
    }

    #[test]
    fn explicit_base_must_match_the_first_parent() {
        let base = "a".repeat(40);
        assert_eq!(
            ConversationEvent::parse(&json!({}))
                .unwrap()
                .boundary(&base),
            Ok(EventBoundary::Ordinary)
        );
        assert_eq!(
            ConversationEvent::parse(&json!({"base": base}))
                .unwrap()
                .boundary(&base),
            Ok(EventBoundary::Root)
        );
        assert!(ConversationEvent::parse(&json!({"base": "b".repeat(40)}))
            .unwrap()
            .boundary(&base)
            .is_err());
        assert_eq!(
            ConversationEvent::parse(&json!({"forked_from": base}))
                .unwrap()
                .boundary(&base),
            Ok(EventBoundary::Fork)
        );
        assert!(
            ConversationEvent::parse(&json!({"base": base, "forked_from": base}))
                .unwrap()
                .boundary(&base)
                .is_err()
        );
    }

    #[test]
    fn task_status_validates_the_complete_spine_before_returning() {
        let head = "a".repeat(40);
        let parent = "b".repeat(40);
        let task = "c".repeat(40);
        let mut cache = HashMap::from([
            (
                head.clone(),
                RemoteCommit {
                    tree: "d".repeat(40),
                    parent: Some(parent.clone()),
                    message: json!({"async": {"task": task, "status": "complete"}}).to_string(),
                },
            ),
            (
                parent,
                RemoteCommit {
                    tree: "e".repeat(40),
                    parent: None,
                    message: "{}".to_string(),
                },
            ),
        ]);

        let error = task_state("unused", &head, &task, &mut cache).unwrap_err();
        assert!(error.contains("no first parent"), "{error}");
    }

    #[test]
    fn target_ref_is_only_a_conversation_head() {
        assert!(validate_target_ref("refs/caos/v2/conversations/chat-1/head").is_ok());
        assert!(validate_target_ref("refs/heads/main").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/chat-1/status").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/a/head/b/head").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/a/title/b/head").is_err());
        assert!(validate_target_ref(&format!(
            "refs/caos/v2/conversations/{}/head",
            "a".repeat(124)
        ))
        .is_ok());
        assert!(validate_target_ref(&format!(
            "refs/caos/v2/conversations/{}/head",
            "a".repeat(125)
        ))
        .is_err());
        assert!(validate_target_ref("refs/caos/conversations/chat-1/head").is_err());
    }

    #[test]
    fn malformed_async_state_is_not_a_terminal_verdict() {
        let task = "a".repeat(40);
        assert_eq!(
            event_task_state(
                &json!({"async": {"task": "oops", "status": "complete"}}),
                &task
            ),
            None
        );
        assert_eq!(
            event_task_state(
                &json!({"async": {
                    "task": task,
                    "status": "failed",
                    "result": "b".repeat(40)
                }}),
                &task
            )
            .as_ref(),
            Some(&TaskState::Terminal(TerminalOutcome {
                status: "failed".to_string(),
                result: "b".repeat(40),
            }))
        );
    }

    #[test]
    fn newest_pending_shadows_an_older_terminal_outcome() {
        let head = "a".repeat(40);
        let root = "b".repeat(40);
        let base = "c".repeat(40);
        let task = "d".repeat(40);
        let result = "e".repeat(40);
        let mut cache = HashMap::from([
            (
                head.clone(),
                RemoteCommit {
                    tree: "f".repeat(40),
                    parent: Some(root.clone()),
                    message: json!({"async": {"task": task, "status": "pending"}}).to_string(),
                },
            ),
            (
                root,
                RemoteCommit {
                    tree: "f".repeat(40),
                    parent: Some(base.clone()),
                    message: json!({
                        "base": base,
                        "async": {"task": task, "status": "complete", "result": result}
                    })
                    .to_string(),
                },
            ),
        ]);

        assert_eq!(
            task_state("unused", &head, &task, &mut cache),
            Ok(Some(TaskState::Pending))
        );
    }

    #[test]
    fn terminal_outcome_identity_includes_status_and_result() {
        let result = "a".repeat(40);
        let failed = TerminalOutcome {
            status: "failed".to_string(),
            result: result.clone(),
        };
        assert_eq!(
            failed,
            TerminalOutcome {
                status: "failed".to_string(),
                result,
            }
        );
        assert_ne!(
            failed,
            TerminalOutcome {
                status: "complete".to_string(),
                result: "a".repeat(40),
            }
        );
        assert_ne!(
            failed,
            TerminalOutcome {
                status: "failed".to_string(),
                result: "b".repeat(40),
            }
        );
    }

    #[test]
    fn immutable_commit_cache_reuses_spine_entries_across_retries() {
        let hash = "a".repeat(40);
        let fetches = std::cell::Cell::new(0);
        let mut cache = HashMap::new();
        let mut fetch = || {
            fetches.set(fetches.get() + 1);
            Ok(RemoteCommit {
                tree: "b".repeat(40),
                parent: None,
                message: "{}".to_string(),
            })
        };

        assert_eq!(
            cached_commit(&mut cache, &hash, &mut fetch).unwrap().tree,
            "b".repeat(40)
        );
        assert_eq!(
            cached_commit(&mut cache, &hash, &mut fetch).unwrap().tree,
            "b".repeat(40)
        );
        assert_eq!(fetches.get(), 1);
    }
}
