//! Model-facing independent work.
//!
//! A call supplies one already-complete CAOS request `R`. We derive the
//! conversation-scoped task request
//!
//! ```text
//! Q = run-and-update-ref { subreq: R, target-ref: F }
//! ```
//!
//! write `.caos/async/Q/status = pending`, ask the caller to durably append
//! that workspace/event, and only then hand Q to `caos run-async`. Keeping the
//! append as a callback avoids coupling this module to the conversation-ref
//! implementation while making the record-before-launch ordering impossible
//! for its caller to reverse accidentally.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use worker_common::{caos, entries, file_name, link, path, prepare_request, scratch, Arg};

pub const TOOL_NAME: &str = "run_async";
const STATUS_COMPONENTS: &[&str] = &[".caos", "async"];

/// The result of admitting one independent task.
pub struct QueuedTask {
    /// Q: both the task identity and the request whose eventual result is read.
    pub task: String,
    /// The status observed or recorded for Q.
    pub status: String,
    /// The workspace carrying that status. This differs from the input only
    /// when this call recorded `pending` for the first time.
    pub workspace: String,
    /// The ordinary model tool_result block to append before the next round.
    pub result: Value,
}

/// Registry entry for the model-facing independent-work tool.
pub fn declaration() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Start an already-constructed CAOS request in the background. The request must be a complete 40-character ArgTree hash. Returns a task hash immediately; that same hash names the eventual CAOS result. Use this only for work that can proceed independently of the current response.",
        "input_schema": {
            "type": "object",
            "properties": {
                "request": {
                    "type": "string",
                    "description": "The complete CAOS request (ArgTree) hash to run independently."
                }
            },
            "required": ["request"]
        }
    })
}

/// Parse a model call, target the current conversation, and queue its request.
///
/// `record_pending(task, workspace)` must append `workspace`'s tree to the
/// authoritative conversation ref. It returns only after that ref
/// update is durable; Q is never dispatched before a successful return.
pub fn queue_call<F>(
    call: &Value,
    conversation: &str,
    run_and_update_ref_image: &str,
    workspace: &str,
    record_pending: F,
) -> Result<QueuedTask, String>
where
    F: FnOnce(&str, &str) -> Result<(), String>,
{
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .ok_or("run_async tool_use block has no string id")?;
    if call.get("name").and_then(Value::as_str) != Some(TOOL_NAME) {
        return Err(format!("async queue received a non-{TOOL_NAME} tool call"));
    }
    let request = call
        .get("input")
        .and_then(|input| input.get("request"))
        .and_then(Value::as_str)
        .ok_or("run_async call has no string `request`")?;
    let target_ref = conversation_ref(conversation)?;
    queue_request(
        id,
        request,
        &target_ref,
        run_and_update_ref_image,
        workspace,
        record_pending,
    )
}

/// Queue an arbitrary complete request. This is also the subagent adapter:
/// after a caller has initialized a child conversation and built its ordinary
/// llm-step request, pass that request as `subrequest` and the parent head as
/// `target_ref`. Child creation is intentionally not hidden in Q.
pub fn queue_request<F>(
    call_id: &str,
    subrequest: &str,
    target_ref: &str,
    run_and_update_ref_image: &str,
    workspace: &str,
    record_pending: F,
) -> Result<QueuedTask, String>
where
    F: FnOnce(&str, &str) -> Result<(), String>,
{
    validate_hash(subrequest, "async subrequest")?;
    validate_target_ref(target_ref)?;

    let task = prepare_request(
        run_and_update_ref_image,
        &[
            ("subreq", Arg::Lit(subrequest)),
            ("target-ref", Arg::Lit(target_ref)),
        ],
    )?;
    validate_hash(&task, "async task")?;

    let existing = task_status(workspace, &task)?;
    let (status, queued_workspace) = match existing.as_deref() {
        None => {
            let pending_workspace = write_status(workspace, &task, "pending")?;
            record_pending(&task, &pending_workspace)?;
            ("pending".to_string(), pending_workspace)
        }
        Some(status @ ("pending" | "complete" | "failed" | "canceled")) => {
            (status.to_string(), workspace.to_string())
        }
        Some(status) => {
            return Err(format!(
                "async task {task} has invalid recorded status {status:?}"
            ))
        }
    };

    // The durable pending event is the source of truth. Admission happens
    // afterward and may fail transiently; turning that failure into a tool
    // failure would consume the model call while leaving Q forever pending.
    // Every later llm-step invocation re-admits latest pending tasks instead.
    if let Some(error) = dispatch_error(&task, &status, dispatch) {
        eprintln!("llm-step: {error}; task remains pending and will be re-admitted");
    }

    Ok(QueuedTask {
        result: result_block(call_id, &task, &status),
        task,
        status,
        workspace: queued_workspace,
    })
}

fn result_block(call_id: &str, task: &str, status: &str) -> Value {
    let text = json!({"task": task, "status": status}).to_string();
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": [{"type": "text", "text": text}],
    })
}

/// Read task states from the one durable location defined by the protocol:
/// `.caos/async/<Q>/status` in the canonical workspace.
pub(crate) fn tasks(workspace: &str) -> Result<Vec<(String, String)>, String> {
    let mut root = PathBuf::from(workspace);
    for component in STATUS_COMPONENTS {
        caos(["get", path(&root)])?;
        if !root.is_dir() {
            return Err(format!("async status parent {} is not a tree", root.display()));
        }
        root.push(component);
        if !root.exists() {
            return Ok(Vec::new());
        }
    }
    caos(["get", path(&root)])?;
    if !root.is_dir() {
        return Err(format!("async status root {} is not a tree", root.display()));
    }
    let mut found = Vec::new();
    for task_path in entries(path(&root))? {
        let task = file_name(&task_path);
        validate_hash(&task, "async task")?;
        let status = task_status(workspace, &task)?
            .ok_or_else(|| format!("async task {task} has no status"))?;
        validate_status(&status)?;
        found.push((task, status));
    }
    found.sort();
    Ok(found)
}

/// Re-admit one durable pending Q after verifying its recorded target. Q
/// already contains the exact historical worker image and subrequest, so
/// recovery must run Q itself rather than rebuilding it with today's image.
pub(crate) fn readmit_task(task: &str, conversation: &str) -> Result<(), String> {
    validate_hash(task, "async task")?;
    let target_ref = conversation_ref(conversation)?;
    let request = fresh("async-request");
    caos(["get-hash", task, &request])?;
    caos(["get", &request])?;
    if !Path::new(&request).is_dir() {
        return Err(format!("async task {task} is not an ArgTree"));
    }
    let subreq = read_literal_arg(&request, task, "subreq")?;
    validate_hash(&subreq, "async subrequest")?;
    let recorded_target = read_literal_arg(&request, task, "target-ref")?;
    if recorded_target != target_ref {
        return Err(format!(
            "async task {task} does not target current conversation ref {target_ref}"
        ));
    }
    dispatch(task)
}

fn read_literal_arg(request: &str, task: &str, name: &str) -> Result<String, String> {
    let argument = Path::new(request).join(name);
    if !argument.exists() {
        return Err(format!("async task {task} has no {name}"));
    }
    caos(["get", path(&argument)])?;
    if argument.is_dir() {
        return Err(format!("async task {task} has a tree-valued {name}"));
    }
    fs::read_to_string(&argument)
        .map(|value| value.trim().to_string())
        .map_err(|error| format!("reading async task {task} {name}: {error}"))
}

/// Rebuild `pending` atop `workspace`, unless a durable status for Q already
/// exists there. `None` means another writer already recorded or superseded
/// pending; `Some` is the new workspace the caller must append.
pub(crate) fn ensure_pending_workspace(
    workspace: &str,
    task: &str,
) -> Result<Option<String>, String> {
    validate_hash(task, "async task")?;
    match task_status(workspace, task)?.as_deref() {
        None => write_status(workspace, task, "pending").map(Some),
        Some("pending" | "complete" | "failed" | "canceled") => Ok(None),
        Some(status) => Err(format!(
            "async task {task} has invalid recorded status {status:?}"
        )),
    }
}

fn conversation_ref(conversation: &str) -> Result<String, String> {
    let target = format!("refs/caos/conversations/{conversation}/head");
    validate_target_ref(&target)?;
    Ok(target)
}

fn validate_target_ref(target: &str) -> Result<(), String> {
    let Some(conversation) = target
        .strip_prefix("refs/caos/conversations/")
        .and_then(|rest| rest.strip_suffix("/head"))
    else {
        return Err(format!("invalid target conversation ref {target:?}"));
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
        return Err(format!("invalid target conversation ref {target:?}"));
    }
    Ok(())
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

fn validate_status(status: &str) -> Result<(), String> {
    if matches!(status, "pending" | "complete" | "failed" | "canceled") {
        Ok(())
    } else {
        Err(format!("invalid async status {status:?}"))
    }
}

fn task_status(workspace: &str, task: &str) -> Result<Option<String>, String> {
    let mut current = PathBuf::from(workspace);
    for component in STATUS_COMPONENTS.iter().copied().chain([task, "status"]) {
        caos(["get", path(&current)])?;
        if !current.is_dir() {
            return Err(format!(
                "async status parent {} is not a tree",
                current.display()
            ));
        }
        current.push(component);
        if !current.exists() {
            return Ok(None);
        }
    }
    caos(["get", path(&current)])?;
    if current.is_dir() {
        return Err(format!(
            "async status {} is a tree, not a blob",
            current.display()
        ));
    }
    fs::read_to_string(&current)
        .map(|status| Some(status.trim().to_string()))
        .map_err(|error| format!("reading async status {}: {error}", current.display()))
}

fn write_status(workspace: &str, task: &str, status: &str) -> Result<String, String> {
    let stage = scratch(&format!("async-status-{}", counter()))?;
    let components = [STATUS_COMPONENTS[0], STATUS_COMPONENTS[1], task, "status"];
    build_level(
        Some(Path::new(workspace)),
        &stage,
        &components,
        status.as_bytes(),
    )?;
    let out = fresh("ws-async");
    caos(["put", path(&stage), &out])?;
    Ok(out)
}

/// Rebuild one path while symlinking every untouched CAS entry. This is the
/// same hash-level tree surgery used by llm-step's inline file tools.
fn build_level(
    source: Option<&Path>,
    destination: &Path,
    components: &[&str],
    content: &[u8],
) -> Result<(), String> {
    if let Some(source) = source {
        caos(["get", path(source)])?;
        if !source.is_dir() {
            return Err(format!(
                "async status parent {} is not a tree",
                source.display()
            ));
        }
        for child in entries(path(source))? {
            if file_name(&child) != components[0] {
                link(&child, destination.join(file_name(&child)))?;
            }
        }
    }

    let target = destination.join(components[0]);
    if components.len() == 1 {
        fs::write(&target, content)
            .map_err(|error| format!("writing {}: {error}", target.display()))?;
        return Ok(());
    }

    fs::create_dir(&target).map_err(|error| format!("creating {}: {error}", target.display()))?;
    let source_child = match source.map(|parent| parent.join(components[0])) {
        Some(path) if path.is_dir() => Some(path),
        Some(path) if path.exists() => {
            return Err(format!(
                "async status parent {} is not a tree",
                path.display()
            ))
        }
        _ => None,
    };
    build_level(source_child.as_deref(), &target, &components[1..], content)
}

fn dispatch(task: &str) -> Result<(), String> {
    let output = Command::new("caos")
        .args(["run-async", task])
        .output()
        .map_err(|error| format!("launching async task {task}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "launching async task {task} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let reply = String::from_utf8(output.stdout)
        .map_err(|error| format!("run-async reply for {task} is not UTF-8: {error}"))?;
    let expected = format!("request {task}");
    if reply.trim() != expected {
        return Err(format!(
            "run-async admitted the wrong task: expected {expected:?}, got {:?}",
            reply.trim()
        ));
    }
    Ok(())
}

fn dispatch_error<F>(task: &str, status: &str, dispatch: F) -> Option<String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    (status != "canceled")
        .then(|| dispatch(task).err())
        .flatten()
}

fn fresh(prefix: &str) -> String {
    format!("/cas/{prefix}-{}", counter())
}

fn counter() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_exposes_only_a_complete_request() {
        let tool = declaration();
        assert_eq!(tool["name"], TOOL_NAME);
        assert_eq!(tool["input_schema"]["required"], json!(["request"]));
        assert!(tool["input_schema"]["properties"].get("request").is_some());
        assert_eq!(
            tool["input_schema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn result_names_q_as_the_task() {
        let task = "a".repeat(40);
        let block = result_block("call-1", &task, "pending");
        assert_eq!(block["tool_use_id"], "call-1");
        assert_eq!(
            block["content"][0]["text"],
            json!({"task": task, "status": "pending"}).to_string()
        );
    }

    #[test]
    fn target_is_always_a_conversation_head() {
        assert_eq!(
            conversation_ref("project/chat-1").unwrap(),
            "refs/caos/conversations/project/chat-1/head"
        );
        assert!(conversation_ref("project/head/chat-1").is_err());
        assert!(validate_target_ref("refs/heads/main").is_err());
    }

    #[test]
    fn hashes_are_exact() {
        assert!(validate_hash(&"a".repeat(40), "request").is_ok());
        assert!(validate_hash(&"a".repeat(39), "request").is_err());
        assert!(validate_hash(&format!("{}g", "a".repeat(39)), "request").is_err());
    }

    #[test]
    fn dispatch_failure_leaves_pending_recoverable() {
        let task = "a".repeat(40);
        let error = dispatch_error(&task, "pending", |_| Err("temporarily unavailable".into()));
        assert_eq!(error.as_deref(), Some("temporarily unavailable"));
    }

    #[test]
    fn canceled_task_is_never_re_admitted() {
        let task = "a".repeat(40);
        let mut called = false;
        assert_eq!(
            dispatch_error(&task, "canceled", |_| {
                called = true;
                Ok(())
            }),
            None
        );
        assert!(!called);
    }
}
