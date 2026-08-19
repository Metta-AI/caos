//! Model-facing independent work.
//!
//! A call supplies one complete CAOS request `R`. We derive
//!
//! ```text
//! Q = run-and-update-ref { subreq: R, target-ref: F }
//! ```
//!
//! record `{async: {task: Q, status: pending}}` on the conversation, then
//! dispatch Q without waiting. Its terminal event records the result object id.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};
use worker_common::{caos, path, prepare_request, Arg};

use crate::{fresh, progress, validate_hash};

pub const TOOL_NAME: &str = "run_async";

pub fn declaration() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Start or inspect an already-constructed CAOS request in the background. The request must be a complete 40-character ArgTree hash already stored in CAOS. Repeating the same request reads the same durable task; once complete, the result includes its result hash. Use this only for independent work whose complete context is in the request: detached work does not inherit secrets, credentials, model settings, or other ambient execution context.",
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
    S: FnOnce(&str) -> Result<TaskState, String>,
{
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .ok_or("run_async tool_use block has no string id")?;
    if call.get("name").and_then(Value::as_str) != Some(TOOL_NAME) {
        return Err(format!("async queue received a non-{TOOL_NAME} tool call"));
    }
    let Some(request) = call
        .get("input")
        .and_then(|input| input.get("request"))
        .and_then(Value::as_str)
    else {
        return Ok(error_block(
            id,
            "run_async needs a string `request` containing a complete CAOS ArgTree hash",
        ));
    };
    queue_request(
        id,
        request,
        &progress::conversation_ref(conversation)?,
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
    S: FnOnce(&str) -> Result<TaskState, String>,
{
    if let Err(error) = validate_subrequest(subrequest) {
        return Ok(error_block(call_id, &error));
    }
    progress::validate_conversation_ref(target_ref)?;
    if let Some(error) = request_tree_error(subrequest)? {
        return Ok(error_block(call_id, &error));
    }

    // `image_arg` records the arg node's hash, so this is an object in the
    // store, not a path.
    let task = prepare_request(
        Arg::Hash(run_and_update_ref_image),
        &[
            ("subreq", Arg::Lit(subrequest)),
            ("target-ref", Arg::Lit(target_ref)),
        ],
    )?;
    validate_hash(&task, "async task")?;

    let state = ensure_status(&task)?;
    validate_state(&state.task, &state.status, state.result.as_deref())?;
    if state.task != task {
        return Err(format!(
            "async status belongs to task {}, expected {task}",
            state.task
        ));
    }

    let should_dispatch = state.status == "pending";
    if let Some(error) = dispatch_error(&task, should_dispatch, dispatch) {
        eprintln!("llm-step: {error}; the durable task state will cause a later recovery to retry");
    }

    Ok(result_block(
        call_id,
        &task,
        &state.status,
        state.result.as_deref(),
    ))
}

/// Check the model-supplied hash before publishing `pending`. A missing object
/// or a non-tree object is user input and becomes an `is_error` result. Failure
/// to launch the client or any server error other than not-found is
/// infrastructure failure and remains fatal to the turn.
fn request_tree_error(request: &str) -> Result<Option<String>, String> {
    if !progress::object_exists(request)? {
        return Ok(Some(format!(
            "async request {request} is not stored in CAOS"
        )));
    }
    let target = fresh("async-subrequest");
    let output = Command::new("caos")
        .args(["get-hash", request, &target])
        .output()
        .map_err(|error| format!("validating async subrequest {request}: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "validating async subrequest {request} failed ({}): {detail}",
            output.status
        ));
    }
    if !Path::new(&target).is_dir() {
        return Ok(Some(format!(
            "async request {request} is not an ArgTree (its object is not a tree)"
        )));
    }
    Ok(None)
}

pub(crate) fn result_block(
    call_id: &str,
    task: &str,
    status: &str,
    result: Option<&str>,
) -> Value {
    let text = json!({"task": task, "status": status, "result": result}).to_string();
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": [{"type": "text", "text": text}],
    })
}

fn error_block(call_id: &str, error: &str) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "is_error": true,
        "content": [{"type": "text", "text": error}],
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskState {
    pub task: String,
    pub status: String,
    pub result: Option<String>,
    pub event_index: usize,
}

/// Fold the latest status for each task from chronological conversation event
/// values. Task state is conversation metadata, not workspace content. A
/// malformed or future-version state cannot be allowed to brick the append-only
/// conversation, so it is ignored with a warning; writers remain strict.
pub(crate) fn tasks<'a>(events: impl IntoIterator<Item = &'a Value>) -> Vec<TaskState> {
    let mut tasks = BTreeMap::new();
    for (event_index, event) in events.into_iter().enumerate() {
        let Some(state) = event.get("async") else {
            continue;
        };
        let parsed = (|| {
            let task = state
                .get("task")
                .and_then(Value::as_str)
                .ok_or("conversation async event has no string task")?;
            let status = state
                .get("status")
                .and_then(Value::as_str)
                .ok_or("conversation async event has no string status")?;
            let result = match state.get("result") {
                Some(result) => Some(
                    result
                        .as_str()
                        .ok_or("conversation async event result is not a string")?,
                ),
                None => None,
            };
            validate_state(task, status, result)?;
            Ok::<_, String>((task, status, result))
        })();
        let (task, status, result) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!(
                    "llm-step: ignoring malformed async event at index {event_index}: {error}"
                );
                continue;
            }
        };
        tasks.insert(
            task.to_string(),
            TaskState {
                task: task.to_string(),
                status: status.to_string(),
                result: result.map(str::to_string),
                event_index,
            },
        );
    }
    tasks.into_values().collect()
}

pub(crate) fn task_status<'a>(
    events: impl IntoIterator<Item = &'a Value>,
    task: &str,
) -> Result<Option<TaskState>, String> {
    validate_hash(task, "async task")?;
    Ok(tasks(events).into_iter().find(|state| state.task == task))
}

/// Re-admit Q only after verifying that its recorded target is this
/// conversation. Q contains the historical worker image and subrequest, so
/// recovery dispatches Q itself rather than rebuilding it.
pub(crate) fn readmit_task(task: &str, conversation: &str) -> Result<(), String> {
    validate_hash(task, "async task")?;
    let target_ref = progress::conversation_ref(conversation)?;
    let (subreq, recorded_target) = task_request(task)?;
    if recorded_target != target_ref {
        return Err(format!(
            "async task {task} does not target current conversation ref {target_ref}"
        ));
    }
    validate_subrequest(&subreq)?;
    dispatch(task)
}

pub(crate) fn task_request(task: &str) -> Result<(String, String), String> {
    validate_hash(task, "async task")?;
    let request = fresh("async-request");
    caos(["get-hash", task, &request])?;
    caos(["get", &request])?;
    if !Path::new(&request).is_dir() {
        return Err(format!("async task {task} is not an ArgTree"));
    }
    let subreq = read_literal_arg(&request, task, "subreq")?;
    validate_subrequest(&subreq)?;
    let recorded_target = read_literal_arg(&request, task, "target-ref")?;
    progress::validate_conversation_ref(&recorded_target)?;
    Ok((subreq, recorded_target))
}

pub(crate) fn request_literal(request: &str, name: &str) -> Result<String, String> {
    validate_hash(request, "request")?;
    let materialized = fresh("async-subrequest-details");
    caos(["get-hash", request, &materialized])?;
    caos(["get", &materialized])?;
    if !Path::new(&materialized).is_dir() {
        return Err(format!("request {request} is not an ArgTree"));
    }
    read_literal_arg(&materialized, request, name)
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

fn validate_status(status: &str) -> Result<(), String> {
    if matches!(status, "pending" | "complete" | "failed") {
        Ok(())
    } else {
        Err(format!("invalid async status {status:?}"))
    }
}

fn validate_state(task: &str, status: &str, result: Option<&str>) -> Result<(), String> {
    validate_hash(task, "async task")?;
    validate_status(status)?;
    match (status, result) {
        ("pending", None) => Ok(()),
        ("pending", Some(_)) => Err("a pending async event must not carry a result".to_string()),
        ("complete" | "failed", Some(result)) => validate_hash(result, "async result"),
        ("complete" | "failed", None) => Err(format!(
            "a terminal async event with status {status:?} needs a result"
        )),
        _ => unreachable!("validate_status accepted only known statuses"),
    }
}

fn validate_subrequest(subrequest: &str) -> Result<(), String> {
    validate_hash(subrequest, "async subrequest")
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

fn dispatch_error<F>(task: &str, needed: bool, dispatch: F) -> Option<String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    needed.then(|| dispatch(task).err()).flatten()
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
        let result = "b".repeat(40);
        let events = [
            json!({"async": {"task": task, "status": "pending"}}),
            json!({"content": "unrelated"}),
            json!({"async": {"task": task, "status": "complete", "result": result}}),
        ];
        let state = task_status(events.iter(), &task).unwrap().unwrap();
        assert_eq!(state.status, "complete");
        assert_eq!(state.result.as_deref(), Some(result.as_str()));
    }

    #[test]
    fn malformed_async_events_are_skipped_without_losing_valid_state() {
        let task = "a".repeat(40);
        let events = [
            json!({"async": {"task": task, "status": "pending"}}),
            json!({"async": {"task": task, "status": "complete"}}),
            json!({"async": {"task": "oops", "status": "pending"}}),
            json!({"async": {"task": task, "status": "future"}}),
        ];
        let state = task_status(events.iter(), &task).unwrap().unwrap();
        assert_eq!(state.status, "pending");
        assert_eq!(state.result, None);
    }

    #[test]
    fn malformed_model_arguments_are_tool_errors() {
        let missing = json!({"type": "tool_use", "id": "call-1", "name": TOOL_NAME, "input": {}});
        let result = queue_call(&missing, "chat", "unused", |_| {
            panic!("malformed call must not record pending")
        })
        .unwrap();
        assert_eq!(result["tool_use_id"], "call-1");
        assert_eq!(result["is_error"], true);

        let malformed = json!({
            "type": "tool_use",
            "id": "call-2",
            "name": TOOL_NAME,
            "input": {"request": "not-a-hash"}
        });
        let result = queue_call(&malformed, "chat", "unused", |_| {
            panic!("malformed call must not record pending")
        })
        .unwrap();
        assert_eq!(result["tool_use_id"], "call-2");
        assert_eq!(result["is_error"], true);

        let uppercase = json!({
            "type": "tool_use",
            "id": "call-3",
            "name": TOOL_NAME,
            "input": {"request": "A".repeat(40)}
        });
        let result = queue_call(&uppercase, "chat", "unused", |_| {
            panic!("uppercase request must not record pending")
        })
        .unwrap();
        assert_eq!(result["tool_use_id"], "call-3");
        assert_eq!(result["is_error"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("lowercase"));
    }

    #[test]
    fn only_pending_state_is_dispatched() {
        let task = "a".repeat(40);
        assert!(dispatch_error(&task, false, |_| Err("called".into())).is_none());
        assert_eq!(
            dispatch_error(&task, true, |_| Err("temporarily unavailable".into())).as_deref(),
            Some("temporarily unavailable")
        );
    }

    #[test]
    fn terminal_events_require_their_result_oid() {
        let task = "a".repeat(40);
        let result = "b".repeat(40);
        assert!(validate_state(&task, "pending", None).is_ok());
        assert!(validate_state(&task, "pending", Some(&result)).is_err());
        assert!(validate_state(&task, "complete", None).is_err());
        assert!(validate_state(&task, "failed", None).is_err());
        assert!(validate_state(&task, "complete", Some(&result)).is_ok());
    }
}
