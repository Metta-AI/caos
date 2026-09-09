//! Model-facing independent work on v3 conversation refs.

use std::process::Command;

use conversation_protocol::v3::{refs, Oid};
use serde_json::{json, Value};
use worker_common::{prepare_request, Arg};

use crate::{error_block, literal_arg_tree, result_block as tool_result};

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

pub fn request(call: &Value) -> Result<&str, Value> {
    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(request) = call
        .get("input")
        .and_then(|input| input.get("request"))
        .and_then(Value::as_str)
    else {
        return Err(error_block(
            id,
            "run_async needs a string `request` containing a complete CAOS ArgTree hash",
        ));
    };
    if let Err(error) = Oid::parse(request, "async subrequest") {
        return Err(error_block(id, &error));
    }
    Ok(request)
}

pub fn prepare_task(
    subrequest: &str,
    target_ref: &str,
    run_and_update_ref_image: &str,
) -> Result<Oid, String> {
    Oid::parse(subrequest, "async subrequest")?;
    refs::parse_head_ref(target_ref)?;
    let task = prepare_request(
        Arg::Hash(run_and_update_ref_image),
        &[
            ("subreq", Arg::Lit(subrequest)),
            ("target-ref", Arg::Lit(target_ref)),
        ],
    )?;
    Oid::parse(&task, "async task")
}

pub fn result_block(call_id: &str, task: &Oid, status: &str, result: Option<&str>) -> Value {
    let text = json!({"task": task.as_str(), "status": status, "result": result}).to_string();
    tool_result(call_id, &text, false)
}

pub fn dispatch(task: &Oid) -> Result<(), String> {
    let output = Command::new("caos")
        .args(["sub-run", task.as_str()])
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
        .map_err(|error| format!("sub-run reply for {task} is not UTF-8: {error}"))?;
    let expected = format!("request {task}");
    if reply.trim() != expected {
        return Err(format!(
            "sub-run dispatched the wrong task: expected {expected:?}, got {:?}",
            reply.trim()
        ));
    }
    Ok(())
}

/// Recovery proves that the immutable task still targets this conversation
/// before redispatching it verbatim.
pub fn task_request(task: &Oid) -> Result<(String, String), String> {
    let mut arguments =
        literal_arg_tree(task, "async task", &["subreq", "target-ref"])?.into_iter();
    let subrequest = arguments.next().unwrap();
    Oid::parse(&subrequest, "async subrequest")?;
    let target_ref = arguments.next().unwrap();
    refs::parse_head_ref(&target_ref)?;
    Ok((subrequest, target_ref))
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
    fn malformed_model_arguments_are_tool_errors() {
        let missing = json!({"type":"tool_use","id":"call-1","name":TOOL_NAME,"input":{}});
        assert_eq!(request(&missing).unwrap_err()["is_error"], true);
        let malformed =
            json!({"type":"tool_use","id":"call-2","name":TOOL_NAME,"input":{"request":"bad"}});
        assert_eq!(request(&malformed).unwrap_err()["tool_use_id"], "call-2");
    }
}
