//! Durable child agents built on the independent-work protocol.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_recurry, cas_hash, entries, file_name, link, own_args_tree, path,
    prepare_request, scratch, write_commit_as, Arg,
};

use crate::{async_work, fresh, progress, read_commit_timestamp};

pub const SPAWN_TOOL: &str = "spawn_agent";
const STEP_DIR: &str = ".caos";
const REPO_AGENT_FILE: &str = "agent.json";

pub fn declarations() -> [Value; 1] {
    [json!({
        "name": SPAWN_TOOL,
        "description": "Start a focused coding agent in a separate visible conversation. It inherits this turn's model and tools, but not the parent transcript or agent-spawning tool. The result includes its conversation, ordinary llm-step request, and async task. Call run_async with that request to read its status and result; merge the completed result commit to apply its files.",
        "input_schema": {
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "A self-contained task with the desired output and constraints."}
            },
            "required": ["prompt"]
        }
    })]
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_call<S>(
    call: &Value,
    parent: &str,
    owner: &str,
    run: &str,
    round: u64,
    ws: &str,
    wc: &str,
    system: &str,
    run_and_update_ref_image: &str,
    ensure_status: S,
) -> Result<Value, String>
where
    S: FnOnce(&str) -> Result<async_work::TaskState, String>,
{
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .ok_or("spawn_agent tool_use block has no string id")?;
    let Some(prompt) = call
        .get("input")
        .and_then(|input| input.get("prompt"))
        .and_then(Value::as_str)
    else {
        return Ok(tool_block(id, "spawn_agent needs a string `prompt`", true));
    };
    if prompt.trim().is_empty() {
        return Ok(tool_block(id, "spawn_agent prompt is empty", true));
    }

    let agent = agent_id(parent, run, round, id)?;
    let subrequest = match progress::agent_request(&agent, prompt)? {
        Some(request) => request,
        None => {
            let (base, tree) = clean_agent_base(ws, wc)?;
            let title = progress::agent_title(prompt);
            let root = progress::store_agent_root(
                &base, &tree, prompt, owner, parent, run, round, id, &title,
            )?;
            let request = child_request(&agent, &root, system)?;
            if progress::create_agent_conversation(
                &agent,
                owner,
                &root,
                &tree,
                &request,
                &title,
            )? {
                request
            } else {
                progress::agent_request(&agent, prompt)?.ok_or_else(|| {
                    format!("agent conversation {agent:?} won creation but disappeared")
                })?
            }
        }
    };

    let mut result = async_work::queue_request(
        id,
        &subrequest,
        &progress::conversation_ref(parent)?,
        run_and_update_ref_image,
        ensure_status,
    )?;
    add_agent_identity(&mut result, &agent, &subrequest)?;
    Ok(result)
}

fn agent_id(parent: &str, run: &str, round: u64, call_id: &str) -> Result<String, String> {
    let identity = serde_json::to_vec(&json!({
        "parent": parent,
        "run": run,
        "round": round,
        "call": call_id,
    }))
    .map_err(|error| format!("serializing agent identity: {error}"))?;
    let dir = scratch("agent-identity")?;
    let source = dir.join("identity");
    fs::write(&source, identity).map_err(|error| format!("writing agent identity: {error}"))?;
    let stored = fresh("agent-identity");
    caos(["put", path(&source), &stored])?;
    Ok(format!("agent-{}", cas_hash(&stored)?))
}

fn clean_agent_base(ws: &str, wc: &str) -> Result<(String, String), String> {
    caos(["get", ws])?;
    let clean = scratch("agent-workspace")?;
    for entry in entries(ws)? {
        let name = file_name(&entry);
        if name == STEP_DIR {
            caos(["get", path(&entry)])?;
            let agent = entry.join(REPO_AGENT_FILE);
            if agent.is_file() {
                let target = clean.join(STEP_DIR);
                fs::create_dir_all(&target)
                    .map_err(|error| format!("creating {}: {error}", target.display()))?;
                link(&agent, target.join(REPO_AGENT_FILE))?;
            }
        } else {
            link(&entry, clean.join(name))?;
        }
    }
    let clean_tree = fresh("agent-workspace");
    caos(["put", path(&clean), &clean_tree])?;
    let tree = cas_hash(&clean_tree)?;
    let timestamp = read_commit_timestamp(wc)?;
    let parent = cas_hash(wc)?;
    let base_path = fresh("agent-base");
    let base = write_commit_as(
        &tree,
        &[&parent],
        "isolated subagent workspace",
        Some(("caos-agent", timestamp)),
        &base_path,
    )?;
    Ok((base, tree))
}

fn child_request(agent: &str, root: &str, system: &str) -> Result<String, String> {
    let root_path = fresh("agent-root");
    caos(["get-hash", root, &root_path])?;
    let child_system = format!(
        "{system}\n\nYou are a focused subagent. Work only on the user's delegated task in this isolated snapshot. Use the available tools, make any requested workspace edits, and finish with a concise report. You cannot spawn further agents."
    );
    const DROP: &[&str] = &[
        "conversation",
        "head",
        "system",
        "merge-refs",
        "wc",
        "run",
        "round",
        "base-head",
        "current-id",
        "current-tool",
        "ws",
        "scope",
        "in",
        "result",
        "error",
    ];
    let unbind = DROP
        .iter()
        .copied()
        .filter(|name| Path::new(&arg(name)).exists())
        .collect::<Vec<_>>();
    let child = caos_recurry(
        Arg::Hash(&own_args_tree()?),
        &unbind,
        &[
            ("conversation", Arg::Lit(agent)),
            ("head", Arg::Path(&root_path)),
            ("subagent", Arg::Lit("true")),
            ("system", Arg::Lit(&child_system)),
        ],
    )?;
    prepare_request(Arg::Hash(&child), &[])
}

fn add_agent_identity(block: &mut Value, agent: &str, request: &str) -> Result<(), String> {
    let text = block
        .pointer_mut("/content/0/text")
        .and_then(|value| value.as_str())
        .ok_or("async result has no text content")?
        .to_string();
    let mut detail: Value = serde_json::from_str(&text)
        .map_err(|error| format!("async result is not JSON: {error}"))?;
    detail["agent"] = Value::String(agent.to_string());
    detail["request"] = Value::String(request.to_string());
    block["content"][0]["text"] = Value::String(detail.to_string());
    Ok(())
}

fn tool_block(call_id: &str, message: &str, is_error: bool) -> Value {
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": [{"type": "text", "text": message}],
    });
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_small_and_explicit() {
        let [spawn] = declarations();
        assert_eq!(spawn["name"], SPAWN_TOOL);
        assert_eq!(spawn["input_schema"]["required"], json!(["prompt"]));
    }

    #[test]
    fn child_title_is_the_normal_prompt_fallback() {
        assert_eq!(
            progress::agent_title("  Inspect\n the delegated snapshot  "),
            "Inspect the delegated snapshot"
        );
        assert_eq!(progress::agent_title(&"界".repeat(61)), format!("{}…", "界".repeat(59)));
    }

    #[test]
    fn spawn_result_names_its_child() {
        let mut result = async_work::result_block(
            "call",
            &"a".repeat(40),
            "pending",
            None,
        );
        add_agent_identity(&mut result, "agent-test", &"b".repeat(40)).unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("agent-test"));
        assert!(text.contains(&"b".repeat(40)));
    }
}
