//! Minimal status-only event append protocol for the finish stage.
//!
//! This intentionally mirrors llm-step's exact-ref client locally: std workers
//! are built as standalone projects and cannot import a module from another std
//! binary. Status appends are tree-neutral, so a CAS loser simply retries from
//! the latest head with that head's tree.

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
        || conversation.len() > 512
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
                || component == "head"
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

    for _ in 0..MAX_CAS_ATTEMPTS {
        let head = read_ref(&base, refname)?
            .ok_or_else(|| format!("target conversation ref {refname} does not exist"))?;
        let remote = fetch_commit(&base, &head)?;
        if matches!(
            task_status(&base, &head, task)?.as_deref(),
            Some("complete" | "failed")
        ) {
            return Ok(());
        }

        let message = json!({"v": 2, "async": {"task": task, "status": status}});
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

fn task_status(base: &str, head: &str, task: &str) -> Result<Option<String>, String> {
    let mut current = Some(head.to_string());
    while let Some(hash) = current {
        let commit = fetch_commit(base, &hash)?;
        let event = match serde_json::from_str::<Value>(commit.message.trim()) {
            Ok(event) if event.get("v").and_then(Value::as_u64) == Some(2) => event,
            _ => return Ok(None),
        };
        if let Some(status) = event_task_status(&event, task) {
            return Ok(Some(status));
        }
        current = commit.parent;
    }
    Ok(None)
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
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ref_is_only_a_conversation_head() {
        assert!(validate_target_ref("refs/caos/v2/conversations/chat-1/head").is_ok());
        assert!(validate_target_ref("refs/heads/main").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/chat-1/status").is_err());
        assert!(validate_target_ref("refs/caos/v2/conversations/a/head/b/head").is_err());
        assert!(validate_target_ref("refs/caos/conversations/chat-1/head").is_err());
    }

    #[test]
    fn malformed_async_state_is_not_a_terminal_verdict() {
        let task = "a".repeat(40);
        assert_eq!(
            event_task_status(
                &json!({"v": 2, "async": {"task": "oops", "status": "complete"}}),
                &task
            ),
            None
        );
        assert_eq!(
            event_task_status(
                &json!({"v": 2, "async": {"task": task, "status": "failed"}}),
                &task
            )
            .as_deref(),
            Some("failed")
        );
    }
}
