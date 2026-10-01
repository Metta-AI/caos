//! Git primitives whose inputs and outputs are object IDs, never checkouts.
use crate::{ServerRequest, Transport};

#[derive(Debug, PartialEq, Eq)]
pub struct MergeTree {
    pub tree: String,
    /// Native merge-tree output after its leading tree ID, including stages.
    pub conflicts: String,
}

pub fn merge_tree(
    transport: &dyn Transport,
    merge_base: Option<&str>,
    ours: &str,
    theirs: &str,
) -> Result<MergeTree, String> {
    for hash in merge_base.into_iter().chain([ours, theirs]) {
        oid(hash)?;
        transport.ensure_pushed(hash)?;
    }
    let request = serde_json::json!({
        "merge_base": merge_base, "ours": ours, "theirs": theirs,
    })
    .to_string();
    let bytes = crate::server_call(
        &transport.server_url()?,
        &ServerRequest {
            method: "POST",
            path: "/git/merge-tree",
            headers: &[("Content-Type", "application/json".into())],
            body: Some(request.as_bytes()),
            timeout_secs: None,
        },
    )?;
    let result: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid merge-tree response: {e}"))?;
    let tree = result["tree"]
        .as_str()
        .ok_or("merge-tree response lacks tree")?;
    oid(tree)?;
    let conflicts = result["conflicts"]
        .as_str()
        .ok_or("merge-tree response lacks conflicts")?;
    Ok(MergeTree {
        tree: tree.to_owned(),
        conflicts: conflicts.to_owned(),
    })
}

fn oid(value: &str) -> Result<String, String> {
    if !git_locator::import::commit(value) {
        return Err("expected a full object hash".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn run_merge(args: &[String]) -> Result<(), String> {
    let (base, ours, theirs) = match args {
        [base, ours, theirs] => (Some(base.as_str()), ours, theirs),
        [ours, theirs] => (None, ours, theirs),
        _ => return Err("git-merge-tree requires <base-tree> <ours-tree> <theirs-tree>, or <ours-commit> <theirs-commit>".into()),
    };
    let result = merge_tree(&crate::HttpTransport::from_env()?, base, ours, theirs)?;
    println!(
        "{}",
        serde_json::json!({"tree":result.tree, "conflicts":result.conflicts})
    );
    Ok(())
}
