//! Host-selected worker tools.
//!
//! A tool set is a tree with one directory per model-facing tool. Each tool
//! contains exactly an Anthropic-compatible `tool.json` declaration and an
//! `image` executor binding. The image is already runnable; llm-step only
//! translates between the model call and the common worker-tool ABI.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use worker_common::{caos, entries, file_name};

const MEMBERS: &[&str] = &["image", "tool.json"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub image: String,
}

impl Tool {
    pub fn declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        })
    }
}

pub fn load(path: Option<&str>) -> Result<Vec<Tool>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    caos(["get", path])?;
    read_tool_set(Path::new(path), true)
}

fn read_tool_set(root: &Path, materialize: bool) -> Result<Vec<Tool>, String> {
    let mut tools = Vec::new();
    for child in entries(&root.to_string_lossy())? {
        let name = file_name(&child);
        validate_name(&name)?;
        if materialize {
            caos(["get", child.to_string_lossy().as_ref()])?;
        }
        if !child.is_dir() {
            return Err(format!("worker tool {name:?} is not a directory"));
        }
        let children = entries(&child.to_string_lossy())?;
        if children.len() != MEMBERS.len()
            || !children
                .iter()
                .all(|entry| MEMBERS.contains(&file_name(entry).as_str()))
        {
            return Err(format!(
                "worker tool {name:?} must contain exactly {}",
                MEMBERS
                    .iter()
                    .map(|member| format!("`{member}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let image = read_leaf(&child, "image", materialize)?;
        let declaration = read_leaf(&child, "tool.json", materialize)?;
        let image = image.trim().to_string();
        let declaration: Value = serde_json::from_str(&declaration)
            .map_err(|error| format!("worker tool {name:?} has invalid tool.json: {error}"))?;
        let object = declaration
            .as_object()
            .ok_or_else(|| format!("worker tool {name:?} tool.json is not an object"))?;
        let expected = ["name", "description", "input_schema"];
        if object.len() != expected.len()
            || !object.keys().all(|key| expected.contains(&key.as_str()))
        {
            return Err(format!(
                "worker tool {name:?} tool.json must contain exactly `name`, `description`, and \
                 `input_schema`"
            ));
        }
        let declared_name = object["name"]
            .as_str()
            .ok_or_else(|| format!("worker tool {name:?} has no string `name`"))?;
        if declared_name != name {
            return Err(format!(
                "worker tool directory {name:?} does not match tool.json name {declared_name:?}"
            ));
        }
        let description = object["description"]
            .as_str()
            .ok_or_else(|| format!("worker tool {name:?} has no string `description`"))?
            .trim()
            .to_string();
        let input_schema = object["input_schema"].clone();
        if description.is_empty() || image.is_empty() {
            return Err(format!(
                "worker tool {name:?} has an empty description or image"
            ));
        }
        if !input_schema.is_object() || input_schema["type"] != "object" {
            return Err(format!(
                "worker tool {name:?} input_schema must be an object with type `object`"
            ));
        }
        tools.push(Tool {
            name,
            description,
            input_schema,
            image,
        });
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tools)
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "invalid worker tool name {name:?}; use 1-64 ASCII letters, digits, `_`, or `-`"
        ));
    }
    Ok(())
}

fn read_leaf(dir: &Path, name: &str, materialize: bool) -> Result<String, String> {
    let path = dir.join(name);
    if materialize {
        caos(["get", path.to_string_lossy().as_ref()])?;
    }
    fs::read_to_string(&path).map_err(|error| format!("reading {}: {error}", path.display()))
}

/// Decode the common worker-tool result and return its model block plus an
/// optional replacement workspace.
pub fn result(id: &str, result: &str) -> Result<(Value, Option<String>), String> {
    caos(["get", result])?;
    if !Path::new(result).is_dir() {
        return Err("worker tool result is not a tree".to_string());
    }
    let result_json = format!("{result}/result.json");
    caos(["get", &result_json])?;
    let value: Value = serde_json::from_str(
        &fs::read_to_string(&result_json)
            .map_err(|error| format!("reading {result_json}: {error}"))?,
    )
    .map_err(|error| format!("worker tool returned invalid result.json: {error}"))?;
    let content = value["content"]
        .as_str()
        .ok_or("worker tool result.json has no string `content`")?;
    let is_error = value["is_error"]
        .as_bool()
        .ok_or("worker tool result.json has no boolean `is_error`")?;
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": [{"type": "text", "text": content}],
    });
    if is_error {
        block["is_error"] = Value::Bool(true);
    }

    let workspace = format!("{result}/workspace");
    if !Path::new(&workspace).exists() {
        return Ok((block, None));
    }
    caos(["get", &workspace])?;
    if Path::new(&workspace).join(".caos").exists() {
        return Err("worker tool returned a workspace containing reserved `.caos`".to_string());
    }
    Ok((block, Some(workspace)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_typed_worker_tool_metadata() {
        let root = std::env::temp_dir().join(format!("caos-worker-tools-{}", std::process::id()));
        let tool = root.join("fixture");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("image"), "abc123\n").unwrap();
        fs::write(
            tool.join("tool.json"),
            r#"{
                "name": "fixture",
                "description": "Fixture tool.",
                "input_schema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}}
                }
            }"#,
        )
        .unwrap();
        let tools = read_tool_set(&root, false).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "fixture");
        assert_eq!(
            tools[0]
                .declaration()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["description", "input_schema", "name"]
        );
        assert_eq!(tools[0].declaration()["input_schema"]["type"], "object");

        fs::remove_dir_all(root).unwrap();
    }
}
