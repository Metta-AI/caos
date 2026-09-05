//! Agent workspace management shares the host's creation rules and atomic append.
use super::*;
use conversation_protocol::v3::WorkspaceBase;

pub(super) fn declaration() -> Value {
    json!({"name":"workspaces", "description":"List workspaces, create a separate change from a named workspace, remove one, or promote a completed subagent result into a visible workspace. New workspaces share the source repository. Set stacked=true only when the new change depends on the source change and should have its own dependent PR. Temporary subagent workspaces remain inside their child conversation until promoted. Never remove a workspace with useful unmerged work without the user's agreement.",
        "input_schema":{"type":"object","properties":{
            "action":{"type":"string","enum":["list","create","remove","promote"]},
            "name":{"type":"string","description":"Workspace to create or remove."},
            "source":{"type":"string","description":"Source workspace name (required for create; child workspace for promote)."},
            "stacked":{"type":"boolean"},
            "child":{"type":"string","description":"Completed subagent ID to promote."}
        },"required":["action"]}})
}

pub(super) fn context(view: &Conversation<'_>, focus: Option<&str>) -> Result<String, String> {
    let workspaces = view.workspaces()?;
    let mut rows = Vec::new();
    for (name, ws) in &workspaces {
        let config = view.workspace_config(name)?;
        rows.push(
            json!({"name":name,"head":ws.commit,"repository":config.repository,"base":config.base}),
        );
    }
    Ok(format!("\n\nWorkspaces: {}\nWorkspace selected when this request started: {}. UI selection changes do not change this request's target. Pass workspace explicitly to tools when there are multiple workspaces. Use workspaces to organize independent changes; use spawn_agent for temporary parallel work and promote only results that deserve separate review.", serde_json::to_string(&rows).map_err(|error| error.to_string())?, focus.unwrap_or("none")))
}

fn required<'a>(input: &'a Value, name: &str) -> Result<&'a str, String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("workspaces needs {name}"))
}

pub(super) fn run(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let planned = plan(&state.conversation()?, &site.call.input, state.store());
    let (mut transitions, text) = match planned {
        Ok(planned) => planned,
        Err(error) => return site.fail(state, &error),
    };
    let stub = site.stub(None);
    let record = completed_record(
        &stub,
        ToolResult::Complete {
            observation: observation_path(&stub),
            proposal: None,
        },
        None,
    );
    transitions.push(tool_complete_transition(
        record,
        &result_block(&site.call.id, &text, false),
        Vec::new(),
    )?);
    let expected = state.head().clone();
    // Both the workspace mutation and its tool receipt land together. A lost
    // response can then be joined by the existing tool-completion check.
    state.try_append_many_at(&expected, transitions)?;
    Ok(())
}

fn plan(
    view: &Conversation<'_>,
    input: &Value,
    store: &dyn ObjectStore,
) -> Result<(Vec<Transition>, String), String> {
    match required(input, "action")? {
        "list" => Ok((Vec::new(), context(view, None)?)),
        "create" => {
            let name = required(input, "name")?;
            let source = required(input, "source")?;
            let transitions = conversation_protocol::v3::workspaces::create_from_workspace(
                view,
                name,
                source,
                input["stacked"].as_bool().unwrap_or(false),
            )?;
            Ok((
                transitions,
                format!("Created workspace {name:?} from {source:?}."),
            ))
        }
        "remove" => {
            let name = required(input, "name")?;
            let mut configs = view.workspace_configs()?;
            configs
                .remove(name)
                .ok_or_else(|| format!("no workspace {name:?}"))?;
            conversation_protocol::v3::workspace_order(&configs)?;
            Ok((
                vec![Transition::WorkspaceRemove {
                    name: name.to_string(),
                }],
                format!("Removed workspace {name:?}."),
            ))
        }
        "promote" => {
            let name = required(input, "name")?;
            paths::validate_workspace_name(name)?;
            if view.workspace(name)?.is_some() {
                return Err(format!("workspace {name:?} already exists"));
            }
            let child = view
                .child(required(input, "child")?)?
                .ok_or("unknown child")?;
            if child.status != ChildStatus::Completed {
                return Err("wait for the subagent to finish before promoting its result".into());
            }
            let source = required(input, "source")?;
            let ws = child
                .child_workspaces
                .as_ref()
                .and_then(|items| items.get(source))
                .ok_or("child has no such workspace")?;
            let terminal = child
                .terminal_head
                .as_ref()
                .ok_or("child is missing its terminal checkpoint")?;
            let mut config = Conversation::open(store, terminal)?.workspace_config(source)?;
            // Child-local dependency names do not identify workspaces in the parent.
            if matches!(config.base, Some(WorkspaceBase::Workspace { .. })) {
                config.base = None;
            }
            if child.spawn_intent.workspace_name.as_deref() == Some(source) {
                if let Some(parent) = child.spawn_intent.workspace_name.as_deref() {
                    if view.workspace(parent)?.is_some()
                        && view.workspace_config(parent)?.repository == config.repository
                    {
                        config.base = child.initial_workspace.as_ref().map(|commit| {
                            WorkspaceBase::Workspace {
                                name: parent.to_string(),
                                commit: commit.clone(),
                            }
                        });
                    }
                }
            }
            config.branch = None;
            Ok((
                {
                    let mut transitions = vec![Transition::WorkspaceCreate {
                        name: name.to_string(),
                        commit: ws.commit.clone(),
                        origin: None,
                    }];
                    if config != Default::default() {
                        transitions.push(Transition::WorkspaceConfigure {
                            name: name.to_string(),
                            config,
                        });
                    }
                    transitions
                },
                format!(
                    "Promoted {source:?} from subagent {} as workspace {name:?}.",
                    child.id
                ),
            ))
        }
        action => Err(format!("unknown workspace action {action:?}")),
    }
}

/// Workspace instructions and schemas apply only to their own repository.
pub(super) fn repository_context(
    view: &Conversation<'_>,
    roots: &[String],
) -> Result<String, String> {
    let mut context = String::new();
    for ((name, _), root) in view.workspaces()?.iter().zip(roots) {
        let rules = Path::new(root).join("AGENTS.md");
        if rules.exists() {
            caos(["get", path(&rules)])?;
            let text =
                fs::read_to_string(&rules).map_err(|e| format!("reading {name}/AGENTS.md: {e}"))?;
            let bounded: String = text.chars().take(64_000).collect();
            context.push_str(&format!("

Repository instructions for workspace {name:?} only (follow applicable nested AGENTS.md when editing):
{bounded}"));
        }
        let tools = tools::tree_tools(root)?
            .iter()
            .map(tools::tree_tool_declaration)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            context.push_str(&format!(
                "

Repository tools for workspace {name:?}; invoke using workspace_tool: {}",
                serde_json::to_string(&tools).map_err(|e| e.to_string())?
            ));
        }
    }
    Ok(context)
}

/// Bind other workspaces by content before ToolStart is committed. The input
/// tree is part of the task's cache key and survives later pointer moves.
pub(super) fn pin_inputs(
    state: &mut progress::State,
    input: &Value,
) -> Result<Option<String>, String> {
    let Some(inputs) = input.get("inputs") else {
        return Ok(None);
    };
    let inputs = inputs.as_object().ok_or("bash inputs must be an object")?;
    let dir = scratch("workspace-inputs")?;
    for (alias, spec) in inputs {
        paths::validate_workspace_name(alias)?;
        let name = spec
            .as_str()
            .or_else(|| spec["workspace"].as_str())
            .ok_or("each input needs a workspace")?;
        let commit = state
            .conversation()?
            .workspace(name)?
            .ok_or_else(|| format!("no input workspace {name:?}"))?
            .commit;
        let (tree, _) = materialize_workspace(state, &commit)?;
        let item = dir.join(alias);
        fs::create_dir(&item).map_err(|e| e.to_string())?;
        link(&tree, item.join("tree"))?;
        let paths = match spec.get("paths") {
            None => ".".to_string(),
            Some(Value::Array(paths)) => {
                let paths = paths
                    .iter()
                    .map(|path| path.as_str().ok_or("input paths must be strings"))
                    .collect::<Result<Vec<_>, _>>()?;
                for path in &paths {
                    if *path != "."
                        && (path.is_empty()
                            || Path::new(path)
                                .components()
                                .any(|part| !matches!(part, std::path::Component::Normal(_))))
                    {
                        return Err(format!("invalid input path {path:?}"));
                    }
                }
                paths.join(
                    "
",
                )
            }
            _ => return Err("input paths must be an array".into()),
        };
        fs::write(item.join("paths"), paths).map_err(|e| e.to_string())?;
    }
    let captured = fresh("workspace-inputs");
    caos(["put", path(&dir), &captured])?;
    Ok(Some(captured))
}
