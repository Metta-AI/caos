//! Docs-driven external tools.
//!
//! A bundle is a tree with one directory per tool; each tool directory contains
//! only a `docs` blob and an `image` blob. The docs are the model-facing
//! description. Every tool uses the same permissive JSON-object input at the
//! API boundary, and its worker interprets that opaque call itself.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use worker_common::{caos, entries, file_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub docs: String,
    pub image: String,
}

impl Tool {
    pub fn declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.docs,
            "input_schema": {
                "type": "object",
                "properties": {},
                "additionalProperties": true
            }
        })
    }
}

pub fn load(path: Option<&str>) -> Result<Vec<Tool>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    caos(["get", path])?;
    read_bundle(Path::new(path), true)
}

fn read_bundle(root: &Path, materialize: bool) -> Result<Vec<Tool>, String> {
    let mut tools = Vec::new();
    for child in entries(&root.to_string_lossy())? {
        let name = file_name(&child);
        validate_name(&name)?;
        if materialize {
            caos(["get", child.to_string_lossy().as_ref()])?;
        }
        if !child.is_dir() {
            return Err(format!("tool {name:?} is not a directory"));
        }
        let children = entries(&child.to_string_lossy())?;
        if children.len() != 2
            || !children
                .iter()
                .all(|entry| matches!(file_name(entry).as_str(), "docs" | "image"))
        {
            return Err(format!(
                "tool {name:?} must contain exactly `docs` and `image` files"
            ));
        }
        let docs = read_leaf(&child, "docs", materialize)?;
        let image = read_leaf(&child, "image", materialize)?;
        if docs.trim().is_empty() {
            return Err(format!("tool {name:?} has empty docs"));
        }
        if image.trim().is_empty() {
            return Err(format!("tool {name:?} has an empty image"));
        }
        tools.push(Tool {
            name,
            docs: docs.trim().to_string(),
            image: image.trim().to_string(),
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
            "invalid tool name {name:?}; use 1-64 ASCII letters, digits, `_`, or `-`"
        ));
    }
    Ok(())
}

fn read_leaf(dir: &Path, name: &str, materialize: bool) -> Result<String, String> {
    let path = dir.join(name);
    if materialize {
        caos(["get", path.to_string_lossy().as_ref()])?;
    }
    fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_docs_and_image_without_a_typed_schema() {
        let root = std::env::temp_dir().join(format!(
            "caos-tool-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cargo = root.join("cargo");
        fs::create_dir_all(&cargo).unwrap();
        fs::write(cargo.join("docs"), "Run Cargo.\n").unwrap();
        fs::write(cargo.join("image"), "abc123\n").unwrap();

        let tools = read_bundle(&root, false).unwrap();
        assert_eq!(
            tools,
            vec![Tool {
                name: "cargo".to_string(),
                docs: "Run Cargo.".to_string(),
                image: "abc123".to_string(),
            }]
        );
        assert_eq!(tools[0].declaration()["input_schema"]["type"], "object");
        assert!(tools[0].declaration()["input_schema"]["properties"]
            .as_object()
            .unwrap()
            .is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unsafe_names_and_extra_files() {
        let root =
            std::env::temp_dir().join(format!("caos-tool-bundle-invalid-{}", std::process::id()));
        let invalid = root.join("bad.name");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("docs"), "Bad.").unwrap();
        fs::write(invalid.join("image"), "abc123").unwrap();
        assert!(read_bundle(&root, false)
            .unwrap_err()
            .contains("invalid tool name"));
        fs::remove_dir_all(&root).unwrap();

        let extra = root.join("valid");
        fs::create_dir_all(&extra).unwrap();
        fs::write(extra.join("docs"), "Valid.").unwrap();
        fs::write(extra.join("image"), "abc123").unwrap();
        fs::write(extra.join("schema"), "{}").unwrap();
        assert!(read_bundle(&root, false)
            .unwrap_err()
            .contains("exactly `docs` and `image`"));
        fs::remove_dir_all(root).unwrap();
    }
}
