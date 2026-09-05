//! Durable child conversations and their model-facing tools.

use std::path::Path;

use conversation_protocol::v3::{refs, Oid};
use serde_json::{json, Value};
use worker_common::{arg, caos, caos_recurry, own_args_tree, prepare_request, Arg};

use crate::{async_work, fresh, literal_arg_tree, result_block};

pub const SPAWN_TOOL: &str = "spawn_agent";
pub const WAIT_TOOL: &str = "wait_agent";
pub const HARVEST_TOOL: &str = "harvest_agent";

const FOCUSED_SYSTEM: &str = "You are a focused subagent. Work only on the user's delegated task in this isolated snapshot. Use the available tools, make any requested workspace edits, and finish with a concise report. You cannot spawn further agents.";

pub fn declarations() -> [Value; 3] {
    [
        json!({
            "name": SPAWN_TOOL,
            "description": "Start a focused coding agent in a durable child conversation. The child receives the selected workspace snapshot and can be joined with wait_agent, then its code can be applied with harvest_agent.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "A self-contained task with the desired output and constraints."},
                    "workspace": {"type": "string", "description": "Workspace to seed. Required when the conversation has several workspaces; omit when it has none."}
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": WAIT_TOOL,
            "description": "Wait for one owned child agent to report a terminal checkpoint. This joins the child's durable relay rather than polling its moving conversation ref.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "child": {"type": "string", "description": "The child id returned by spawn_agent."}
                },
                "required": ["child"]
            }
        }),
        json!({
            "name": HARVEST_TOOL,
            "description": "Apply a terminal child agent's named workspace to a parent workspace with the ordinary three-way reconciliation rules.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "child": {"type": "string", "description": "The terminal child id returned by spawn_agent."},
                    "child_workspace": {"type": "string", "description": "Child workspace to apply; defaults to the workspace seeded at spawn."},
                    "workspace": {"type": "string", "description": "Parent workspace to update; defaults when the parent has exactly one workspace."}
                },
                "required": ["child"]
            }
        }),
    ]
}

pub fn required_string<'a>(call: &'a Value, name: &str, tool: &str) -> Result<&'a str, String> {
    call.get("input")
        .and_then(|input| input.get(name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{tool} needs a non-empty string `{name}`"))
}

pub fn optional_string<'a>(call: &'a Value, name: &str) -> Option<&'a str> {
    call.get("input")
        .and_then(|input| input.get(name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn agent_title(prompt: &str) -> String {
    const MAX_CHARS: usize = 60;
    let compact = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        compact
            .chars()
            .take(MAX_CHARS - 1)
            .chain(std::iter::once('…'))
            .collect()
    }
}

/// Build the child's stable model configuration and its complete runnable
/// request. The child prompt commit is already published before this runs, so
/// `prepare-request` can resolve the commit-kinded `head` argument.
pub fn child_request(
    child: &str,
    prompt_head: &Oid,
    parent_system: &str,
) -> Result<(Oid, Oid), String> {
    const DROP: &[&str] = &[
        "conversation",
        "head",
        "system",
        "subagent",
        "merge-refs",
        "repository-refs",
        "focus-workspace",
        "wc",
        "run",
        "round",
        "base-head",
        "current-id",
        "current-tool",
        "ws",
        "scope",
        "tool-eval",
        "tool-args",
        "tool-git",
        "in",
        "result",
        "error",
    ];
    let unbind = DROP
        .iter()
        .copied()
        .filter(|name| Path::new(&arg(name)).exists())
        .collect::<Vec<_>>();
    let child_system = format!("{parent_system}\n\n{FOCUSED_SYSTEM}");
    // `head` must be a commit-kinded arg (the worker reads it with `cas_hash`
    // and the server refuses a bare hash that is not a tree or blob), so the
    // prompt commit is materialized at a /cas path first, as the client does.
    let head_path = fresh("subagent-head");
    caos(["get-hash", prompt_head.as_str(), &head_path])?;
    let configuration = caos_recurry(
        Arg::Hash(&own_args_tree()?),
        &unbind,
        &[
            ("conversation", Arg::Lit(child)),
            ("head", Arg::Path(&head_path)),
            ("subagent", Arg::Lit("true")),
            ("system", Arg::Lit(&child_system)),
        ],
    )?;
    let request = prepare_request(Arg::Hash(&configuration), &[])?;
    Ok((
        Oid::parse(&configuration, "child configuration")?,
        Oid::parse(&request, "child request")?,
    ))
}

pub fn prepare_relay(
    subrequest: &Oid,
    target_ref: &str,
    child: &str,
    run_and_update_ref_image: &str,
) -> Result<Oid, String> {
    refs::parse_head_ref(target_ref)?;
    refs::head_ref(child)?;
    let relay = prepare_request(
        Arg::Hash(run_and_update_ref_image),
        &[
            ("subreq", Arg::Lit(subrequest.as_str())),
            ("target-ref", Arg::Lit(target_ref)),
            ("child", Arg::Lit(child)),
        ],
    )?;
    Oid::parse(&relay, "subagent relay")
}

/// Recovery verifies the immutable relay key before redispatching it.
pub fn relay_request(relay: &Oid) -> Result<(Oid, String, String), String> {
    let mut arguments =
        literal_arg_tree(relay, "subagent relay", &["subreq", "target-ref", "child"])?.into_iter();
    let subrequest = arguments.next().unwrap();
    let target_ref = arguments.next().unwrap();
    let child = arguments.next().unwrap();
    let subrequest = Oid::parse(&subrequest, "subagent request")?;
    refs::parse_head_ref(&target_ref)?;
    refs::head_ref(&child)?;
    Ok((subrequest, target_ref, child))
}

pub fn dispatch(relay: &Oid) -> Result<(), String> {
    async_work::dispatch(relay)
}

pub fn spawn_observation(child: &str, initial_head: &Oid, request: &Oid) -> Value {
    json!({
        "kind": "subagent-spawned",
        "child": child,
        "initial_head": initial_head.as_str(),
        "request": request.as_str(),
    })
}

pub fn spawn_result(call_id: &str, child: &str, request: &Oid, relay: &Oid) -> Value {
    result_block(
        call_id,
        &json!({
            "child": child,
            "request": request.as_str(),
            "task": relay.as_str(),
        })
        .to_string(),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_are_small_and_explicit() {
        let [spawn, wait, harvest] = declarations();
        assert_eq!(spawn["name"], SPAWN_TOOL);
        assert_eq!(spawn["input_schema"]["required"], json!(["prompt"]));
        assert_eq!(wait["input_schema"]["required"], json!(["child"]));
        assert_eq!(harvest["input_schema"]["required"], json!(["child"]));
    }

    #[test]
    fn child_title_uses_the_conversation_fallback_rule() {
        assert_eq!(
            agent_title("  Inspect\n the delegated snapshot  "),
            "Inspect the delegated snapshot"
        );
        assert_eq!(
            agent_title(&"界".repeat(61)),
            format!("{}…", "界".repeat(59))
        );
    }

    #[test]
    fn spawn_observation_and_model_result_have_distinct_contracts() {
        let initial = Oid::parse(&"a".repeat(40), "initial").unwrap();
        let request = Oid::parse(&"b".repeat(40), "request").unwrap();
        let relay = Oid::parse(&"c".repeat(40), "relay").unwrap();
        assert_eq!(
            spawn_observation("subagent-test", &initial, &request),
            json!({
                "kind":"subagent-spawned",
                "child":"subagent-test",
                "initial_head":initial.as_str(),
                "request":request.as_str(),
            })
        );
        let block = spawn_result("call", "subagent-test", &request, &relay);
        assert!(block["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(relay.as_str()));
    }
}
