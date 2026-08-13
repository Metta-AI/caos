//! Model-facing independent work.
//!
//! A call supplies one complete CAOS request `R`. We derive
//!
//! ```text
//! Q = run-and-update-ref { subreq: R, target-ref: F }
//! ```
//!
//! record `{async: {task: Q, status: pending}}` on the conversation, then
//! dispatch Q without waiting. Q itself names the eventual result.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{json, Value};
use worker_common::{caos, path, prepare_request, Arg};

pub const TOOL_NAME: &str = "run_async";

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

/// Record-before-dispatch is expressed by a callback so this module does not
/// own the conversation append protocol. It returns the existing status or
/// records and returns `pending`. A failed dispatch leaves that durable event
/// for recovery to retry.
pub fn queue_call<S>(
    call: &Value,
    conversation: &str,
    run_and_update_ref_image: &str,
    ensure_status: S,
) -> Result<Value, String>
where
    S: FnOnce(&str) -> Result<String, String>,
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
    queue_request(
        id,
        request,
        &conversation_ref(conversation)?,
        run_and_update_ref_image,
        ensure_status,
    )
}

/// Queue an arbitrary complete request. This is also the subagent adapter: R
/// may be a child conversation's ordinary llm-step request.
pub fn queue_request<S>(
    call_id: &str,
    subrequest: &str,
    target_ref: &str,
    run_and_update_ref_image: &str,
    ensure_status: S,
) -> Result<Value, String>
where
    S: FnOnce(&str) -> Result<String, String>,
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

    let status = ensure_status(&task)?;
    validate_status(&status)?;

    if let Some(error) = dispatch_error(&task, &status, dispatch) {
        eprintln!("llm-step: {error}; task remains pending and will be re-admitted");
    }

    Ok(result_block(call_id, &task, &status))
}

fn result_block(call_id: &str, task: &str, status: &str) -> Value {
    let text = json!({"task": task, "status": status}).to_string();
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": [{"type": "text", "text": text}],
    })
}

/// Fold the latest status for each task from chronological conversation event
/// values. Task state is conversation metadata, not workspace content.
pub(crate) fn tasks<'a>(
    events: impl IntoIterator<Item = &'a Value>,
) -> Result<Vec<(String, String)>, String> {
    let mut tasks = BTreeMap::new();
    for event in events {
        let Some(state) = event.get("async") else {
            continue;
        };
        let task = state
            .get("task")
            .and_then(Value::as_str)
            .ok_or("conversation async event has no string task")?;
        let status = state
            .get("status")
            .and_then(Value::as_str)
            .ok_or("conversation async event has no string status")?;
        validate_hash(task, "async task")?;
        validate_status(status)?;
        tasks.insert(task.to_string(), status.to_string());
    }
    Ok(tasks.into_iter().collect())
}

pub(crate) fn task_status<'a>(
    events: impl IntoIterator<Item = &'a Value>,
    task: &str,
) -> Result<Option<String>, String> {
    validate_hash(task, "async task")?;
    Ok(tasks(events)?
        .into_iter()
        .find_map(|(found, status)| (found == task).then_some(status)))
}

/// Re-admit Q only after verifying that its recorded target is this
/// conversation. Q contains the historical worker image and subrequest, so
/// recovery dispatches Q itself rather than rebuilding it.
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

fn conversation_ref(conversation: &str) -> Result<String, String> {
    let target = format!("refs/caos/v2/conversations/{conversation}/head");
    validate_target_ref(&target)?;
    Ok(target)
}

fn validate_target_ref(target: &str) -> Result<(), String> {
    let Some(conversation) = target
        .strip_prefix("refs/caos/v2/conversations/")
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
    if matches!(status, "pending" | "complete" | "failed") {
        Ok(())
    } else {
        Err(format!("invalid async status {status:?}"))
    }
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
            "run-async dispatched the wrong task: expected {expected:?}, got {:?}",
            reply.trim()
        ));
    }
    Ok(())
}

fn dispatch_error<F>(task: &str, status: &str, dispatch: F) -> Option<String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    (status == "pending")
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
        assert_eq!(
            tool["input_schema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn task_status_is_folded_from_events() {
        let task = "a".repeat(40);
        let events = [
            json!({"v": 2, "async": {"task": task, "status": "pending"}}),
            json!({"v": 2, "content": "unrelated"}),
            json!({"v": 2, "async": {"task": task, "status": "complete"}}),
        ];
        assert_eq!(
            task_status(events.iter(), &task).unwrap().as_deref(),
            Some("complete")
        );
    }

    #[test]
    fn only_pending_tasks_are_dispatched() {
        let task = "a".repeat(40);
        assert!(dispatch_error(&task, "complete", |_| Err("called".into())).is_none());
        assert_eq!(
            dispatch_error(&task, "pending", |_| Err("temporarily unavailable".into())).as_deref(),
            Some("temporarily unavailable")
        );
    }
}
