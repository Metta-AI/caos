//! Minimal status-only event append protocol for the finish stage.
//!
//! This intentionally mirrors llm-step's exact-ref client locally: std workers
//! are built as standalone projects and cannot import a module from another std
//! binary. Status appends are tree-neutral, so a CAS loser simply retries from
//! the latest head with that head's tree.

use std::collections::HashMap;

use serde_json::{json, Value};

const MAX_CAS_ATTEMPTS: usize = 32;

enum PushResult {
    Updated,
    Rejected(String),
}

pub fn validate_target_ref(refname: &str) -> Result<(), String> {
    let Some(rest) = refname.strip_prefix("refs/caos/v2/conversations/") else {
        return Err(format!(
            "target-ref is not a conversation head: {refname:?}"
        ));
    };
    let Some(conversation) = rest.strip_suffix("/head") else {
        return Err(format!(
            "target-ref is not a conversation head: {refname:?}"
        ));
    };
    if conversation.is_empty()
        || conversation.len() > 124
        || conversation.starts_with('/')
        || conversation.ends_with('/')
        || conversation.contains("//")
        || conversation.contains("..")
        || conversation.contains("@{")
        || conversation.ends_with('.')
        || conversation.bytes().any(|b| {
            b <= b' ' || b == 0x7f || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        || conversation.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || matches!(component, "head" | "title")
                || component.starts_with('.')
                || component.ends_with(".lock")
        })
    {
        return Err(format!("invalid target conversation ref {refname:?}"));
    }
    Ok(())
}

pub fn append_status(refname: &str, task: &str, status: &str) -> Result<(), String> {
    validate_target_ref(refname)?;
    validate_hash(task, "task")?;
    if !matches!(status, "complete" | "failed") {
        return Err(format!("invalid async status {status:?}"));
    }
    let base = server_base()?;
    let mut commits = HashMap::new();

    for _ in 0..MAX_CAS_ATTEMPTS {
        let head = read_ref(&base, refname)?
            .ok_or_else(|| format!("target conversation ref {refname} does not exist"))?;
        let remote = fetch_commit_cached(&base, &head, &mut commits)?;
        // A retry after a caught failure can legitimately succeed: caught
        // failures are not cached, so rerunning the same Q may produce the
        // other terminal outcome. Only the same outcome is idempotent. A
        // different one must become the latest durable state so F agrees with
        // the result this execution of Q will return and pin.
        if terminal_status_is_current(
            task_status(&base, &head, task, &mut commits)?.as_deref(),
            status,
        ) {
            return Ok(());
        }

        let message = json!({"async": {"task": task, "status": status}});
        let message = serde_json::to_string(&message)
            .map_err(|error| format!("serializing async status event: {error}"))?;
        let commit = store_commit(&base, &remote.tree, &head, &message)?;
        match push_ref(&base, refname, &head, &commit) {
            Ok(PushResult::Updated) => return Ok(()),
            Ok(PushResult::Rejected(report)) => {
                let observed = read_ref(&base, refname)?;
                if observed.as_deref() == Some(commit.as_str()) {
                    return Ok(());
                }
                if observed.as_deref() == Some(head.as_str()) {
                    return Err(format!(
                        "server rejected update of {refname} without a CAS race: {report}"
                    ));
                }
            }
            Err(error) => {
                let observed = read_ref(&base, refname).map_err(|read_error| {
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

fn terminal_status_is_current(current: Option<&str>, next: &str) -> bool {
    current == Some(next)
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

fn task_status(
    base: &str,
    head: &str,
    task: &str,
    cache: &mut HashMap<String, RemoteCommit>,
) -> Result<Option<String>, String> {
    let mut current = head.to_string();
    let mut newest_status = None;
    loop {
        let hash = current;
        let commit = fetch_commit_cached(base, &hash, cache)?;
        let event = parse_spine_event(&commit.message, &hash)?;
        let parent = required_event_parent(commit.parent.as_deref(), &hash)?;
        let is_root = validate_event_base(&event, &parent)?;
        validate_fork_marker(&event, &parent)?;
        if newest_status.is_none() {
            newest_status = event_task_status(&event, task);
        }
        if is_root {
            return Ok(newest_status);
        }
        current = parent;
    }
}

fn parse_spine_event(
    message: &str,
    commit: &str,
) -> Result<Value, String> {
    let event = serde_json::from_str::<Value>(message.trim())
        .map_err(|error| format!("conversation history commit {commit} is not JSON: {error}"))?;
    let object = event
        .as_object()
        .ok_or_else(|| format!("conversation event {commit} must be a JSON object"))?;
    if object.contains_key("v") {
        return Err(format!(
            "conversation event {commit} must not carry a version; refs/caos/v2 selects the protocol"
        ));
    }
    Ok(event)
}

fn validate_event_base(event: &Value, first_parent: &str) -> Result<bool, String> {
    let Some(base) = event.get("base") else {
        return Ok(false);
    };
    let base = base
        .as_str()
        .ok_or_else(|| "conversation event base is not a string".to_string())?;
    validate_hash(base, "conversation base")?;
    if base != first_parent {
        return Err(format!(
            "conversation root declares base {base}, but its first parent is {first_parent}"
        ));
    }
    Ok(true)
}

fn validate_fork_marker(event: &Value, first_parent: &str) -> Result<bool, String> {
    let Some(forked_from) = event.get("forked_from") else {
        return Ok(false);
    };
    if event.get("base").is_some() {
        return Err("a conversation fork marker must not introduce a new base".to_string());
    }
    let forked_from = forked_from
        .as_str()
        .ok_or_else(|| "conversation event forked_from is not a string".to_string())?;
    validate_hash(forked_from, "forked_from")?;
    if forked_from != first_parent {
        return Err(format!(
            "conversation fork marker declares {forked_from}, but its first parent is {first_parent}"
        ));
    }
    Ok(true)
}

fn required_event_parent(parent: Option<&str>, event: &str) -> Result<String, String> {
    parent
        .map(str::to_string)
        .ok_or_else(|| format!("conversation event {event} has no first parent"))
}

/// Read one event defensively. Conversation history is append-only, so a
/// malformed or future async payload must not prevent a valid finish event
/// from being appended after it.
fn event_task_status(event: &Value, wanted: &str) -> Option<String> {
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
        Ok::<_, String>((task, status))
    })();
    match parsed {
        Ok((task, status)) if task == wanted => Some(status.to_string()),
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

fn push_ref(base: &str, refname: &str, old: &str, new: &str) -> Result<PushResult, String> {
    validate_hash(old, "expected ref")?;
    validate_hash(new, "new ref")?;

    let body = serde_json::to_vec(&json!({
        "ref": refname,
        "expected": old,
        "new": new,
    }))
    .map_err(|error| format!("serializing ref append: {error}"))?;
    let url = format!("{base}/ref/append");
    let response = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if (200..300).contains(&response.status_code) {
        Ok(PushResult::Updated)
    } else if response.status_code == 409 {
        Ok(PushResult::Rejected(
            String::from_utf8_lossy(response.as_bytes())
                .trim()
                .to_string(),
        ))
    } else {
        Err(format!(
            "POST {url}: {} {}: {}",
            response.status_code,
            response.reason_phrase,
            String::from_utf8_lossy(response.as_bytes()).trim()
        ))
    }
}

fn read_ref(base: &str, refname: &str) -> Result<Option<String>, String> {
    let body = serde_json::to_vec(&json!({"ref": refname}))
        .map_err(|error| format!("serializing ref read: {error}"))?;
    let url = format!("{base}/ref/read");
    let response = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if response.status_code == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "POST {url}: {} {}: {}",
            response.status_code,
            response.reason_phrase,
            String::from_utf8_lossy(response.as_bytes()).trim()
        ));
    }
    let hash = response
        .as_str()
        .map_err(|error| format!("POST {url}: {error}"))?
        .trim();
    validate_hash(hash, "remote ref")?;
    Ok(Some(hash.to_string()))
}

pub(crate) fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(format!(
            "{what} must be a lowercase 40-character hexadecimal hash, got {hash:?}"
        ));
    }
    Ok(())
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
        assert_eq!(validate_event_base(&json!({}), &base), Ok(false));
        assert_eq!(
            validate_event_base(&json!({"base": base}), &base),
            Ok(true)
        );
        assert!(validate_event_base(&json!({"base": "b".repeat(40)}), &base).is_err());
        assert!(validate_fork_marker(&json!({"forked_from": base}), &base).is_ok());
        assert!(validate_fork_marker(
            &json!({"base": base, "forked_from": base}),
            &base
        )
        .is_err());
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
                    message: json!({"async": {"task": task, "status": "complete"}})
                        .to_string(),
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

        let error = task_status("unused", &head, &task, &mut cache).unwrap_err();
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
            event_task_status(
                &json!({"async": {"task": "oops", "status": "complete"}}),
                &task
            ),
            None
        );
        assert_eq!(
            event_task_status(
                &json!({"async": {"task": task, "status": "failed"}}),
                &task
            )
            .as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn only_the_same_terminal_outcome_is_idempotent() {
        assert!(terminal_status_is_current(Some("failed"), "failed"));
        assert!(terminal_status_is_current(Some("complete"), "complete"));
        assert!(!terminal_status_is_current(Some("failed"), "complete"));
        assert!(!terminal_status_is_current(Some("complete"), "failed"));
        assert!(!terminal_status_is_current(Some("pending"), "complete"));
        assert!(!terminal_status_is_current(None, "failed"));
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
