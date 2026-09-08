//! Agent workspace management shares the host's creation rules and atomic append.
use super::*;

pub(super) fn declaration() -> Value {
    json!({"name":"workspaces", "description":"Browse and edit code commit references in the conversation tree. Names are paths, not registered workspace objects. Group a stack as feature/.base-url (repository URL and base branch on two lines), feature/00-base, numbered PR boundaries, and one feature/dirty. Edit dirty; move it to 01-description when ready for review, then copy that boundary to a new dirty for further work. Publishing uses numbered paths as branch names. Creating/copying/moving refs does not merge code: merge subagent changes before sealing boundaries.",
        "input_schema":{"type":"object","properties":{
            "action":{"type":"string","enum":["list","create","move","remove","promote"]},
            "name":{"type":"string","description":"Destination commit-entry path."},
            "source":{"type":"string","description":"Source commit-entry path; child path for promote."},
            "child":{"type":"string","description":"Completed subagent ID for promote."}
        },"required":["action"]}})
}

pub(super) fn context(view: &Conversation<'_>, focus: Option<&str>) -> Result<String, String> {
    let workspaces = view.workspaces()?;
    let mut rows = Vec::new();
    for (name, ws) in &workspaces {
        let config = view.workspace_config(name)?;
        rows.push(
            json!({"name":name,"head":ws.commit,"repository":config.repository(),"base":config.upstream}),
        );
    }
    Ok(format!("\n\nWorkspaces: {}\nWorkspace selected when this request started: {}. UI selection changes do not change this request's target. Pass workspace explicitly to tools when there are multiple workspaces. Use commit-entry paths to organize code. Edit dirty and move it to a numbered boundary when ready for review. Use spawn_agent for parallel work; merge its result into dirty or copy it into a review boundary.", serde_json::to_string(&rows).map_err(|error| error.to_string())?, focus.unwrap_or("none")))
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
    _store: &dyn ObjectStore,
) -> Result<(Vec<Transition>, String), String> {
    match required(input, "action")? {
        "list" => Ok((Vec::new(), context(view, None)?)),
        "create" | "move" => {
            let name = required(input, "name")?;
            let source = required(input, "source")?;
            paths::validate_workspace_name(name)?;
            if view.snapshot().exists(name)? {
                return Err("destination already exists".into());
            }
            let ws = view
                .workspace(source)?
                .ok_or("source is not a commit entry")?;
            let mut files = Vec::new();
            if input["action"] == "move" {
                files.push((source.to_string(), None));
            }
            files.push((
                name.to_string(),
                Some((Mode::Commit, ws.commit.encode_line())),
            ));
            Ok((
                vec![Transition::FilesApply { files }],
                format!("Updated {name}."),
            ))
        }
        "remove" => {
            let name = required(input, "name")?;
            view.workspace(name)?.ok_or("no such commit entry")?;
            Ok((
                vec![Transition::reference(name.to_string(), None)],
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
            if child.status != TaskStatus::Complete {
                return Err("wait for the subagent to finish before promoting its result".into());
            }
            let source = required(input, "source")?;
            let ws = child
                .child_workspaces
                .as_ref()
                .and_then(|items| items.get(source))
                .ok_or("child has no such workspace")?;
            Ok((
                {
                    let transitions = vec![Transition::reference(
                        name.to_string(),
                        Some(ws.commit.clone()),
                    )];
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
