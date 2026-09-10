//! Repository context derived from the conversation tree.
use super::*;

pub(super) fn context(view: &Conversation<'_>, focus: Option<&str>) -> Result<String, String> {
    let source_trees = view.source_trees()?;
    let mut rows = Vec::new();
    for (name, ws) in &source_trees {
        let config = view.source_tree_config(name)?;
        rows.push(
            json!({"name":name,"head":ws.commit,"repository":config.repository(),"base":config.upstream}),
        );
    }
    Ok(format!("\n\nSource trees: {}\nSourceTree selected when this request started: {}. UI selection changes do not change this request's target. File tools, grep, and bash start at the conversation root; use full paths through source trees. Bash can edit ordinary conversation files and multiple source trees in one call. Use commit-entry paths to organize code. Edit dirty and move it to a numbered boundary when ready for review. Use spawn_agent for parallel work; merge its result into dirty or copy it into a review boundary.", serde_json::to_string(&rows).map_err(|error| error.to_string())?, focus.unwrap_or("none")))
}

pub(super) fn repository_context(
    view: &Conversation<'_>,
    roots: &[String],
) -> Result<String, String> {
    let mut context = String::new();
    for ((name, _), root) in view.source_trees()?.iter().zip(roots) {
        let rules = Path::new(root).join("AGENTS.md");
        if rules.exists() {
            caos(["get", path(&rules)])?;
            let text =
                fs::read_to_string(&rules).map_err(|e| format!("reading {name}/AGENTS.md: {e}"))?;
            let bounded: String = text.chars().take(64_000).collect();
            context.push_str(&format!("

Repository instructions for source tree {name:?} only (follow applicable nested AGENTS.md when editing):
{bounded}"));
        }
        let tools = tools::tree_tools(root)?
            .iter()
            .map(tools::tree_tool_declaration)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            context.push_str(&format!(
                "

Repository tools for source tree {name:?}; invoke using run_tool with path {name}/caos-tools/<tool-name>: {}",
                serde_json::to_string(&tools).map_err(|e| e.to_string())?
            ));
        }
    }
    Ok(context)
}
