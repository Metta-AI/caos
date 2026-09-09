use super::oid::hex_lower;

pub const FORMAT: &str = ".caos/format";
pub const FORMAT_BYTES: &str = "caos-conversation-v3\n";
pub const IDENTITY: &str = ".caos/identity.json";
pub const TITLE: &str = ".caos/title";
pub const WORKSPACES_DIR: &str = ".caos/workspaces";
pub const TRANSCRIPT_DIR: &str = ".caos/transcript";
pub const REQUESTS_DIR: &str = ".caos/requests";
pub const ACTIVE_REQUEST: &str = ".caos/requests/active";
pub const TOOLS_DIR: &str = ".caos/tools";
pub const ASYNC_DIR: &str = ".caos/async";
pub const SUBAGENTS_DIR: &str = ".caos/subagents";
pub const PUBLICATIONS_DIR: &str = ".caos/publications";
pub const FILES_DIR: &str = "files";
pub const CAOS_DIR: &str = ".caos";
pub const CONFLICTS_LEDGER: &str = ".caos/conflicts";

const MAX_PATH_COMPONENT: usize = 255;
pub const MAX_PATH: usize = 4096;
const MAX_TREE_DEPTH: usize = 64;
pub const MAX_WORKSPACE_NAME: usize = 64;
pub const MAX_TITLE: usize = 1024;
pub const MAX_JSON_INT: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceFile {
    Commit,
    Initial,
    Origin,
}

pub fn validate_component(name: &str) -> Result<(), String> {
    let lower = name.to_ascii_lowercase();
    if name.is_empty()
        || name.len() > MAX_PATH_COMPONENT
        || matches!(name, "." | "..")
        || name.bytes().any(|byte| matches!(byte, b'/' | 0))
        || lower == ".git"
        || lower == "git~1"
        || name.ends_with(['.', ' '])
    {
        return Err(format!("invalid tree path component {name:?}"));
    }
    Ok(())
}

pub fn validate_tree_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_PATH || path.starts_with('/') || path.ends_with('/') {
        return Err(format!("invalid tree path {path:?}"));
    }
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > MAX_TREE_DEPTH {
        return Err(format!("invalid tree path {path:?}"));
    }
    for component in components {
        validate_component(component).map_err(|_| format!("invalid tree path {path:?}"))?;
    }
    Ok(())
}

pub(crate) fn validate_unique(values: &[String], what: &str) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(format!("duplicate {what} {value:?}"));
        }
    }
    Ok(())
}

pub fn validate_workspace_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_WORKSPACE_NAME
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!("invalid workspace name {name:?}"));
    }
    Ok(())
}

pub fn validate_protocol_id_component(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_PATH_COMPONENT
        || id.starts_with('.')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!("invalid protocol id component {id:?}"));
    }
    Ok(())
}

pub fn admit_external_id(id: &str) -> String {
    if validate_protocol_id_component(id).is_ok() {
        id.to_string()
    } else {
        hex_lower(id.as_bytes())
    }
}

pub fn workspace_dir(name: &str) -> String {
    format!("{WORKSPACES_DIR}/{name}")
}

pub fn workspace_commit_path(name: &str) -> String {
    format!("{}/{name}/commit", WORKSPACES_DIR)
}

pub fn workspace_initial_path(name: &str) -> String {
    format!("{}/{name}/initial", WORKSPACES_DIR)
}

pub fn workspace_origin_path(name: &str) -> String {
    format!("{}/{name}/origin", WORKSPACES_DIR)
}

pub fn transcript_shard(ordinal: u64) -> String {
    format!("{:09}", ordinal / 1000)
}

pub fn transcript_entry_path(ordinal: u64, message_id: &str) -> String {
    format!(
        "{TRANSCRIPT_DIR}/{}/{ordinal:012}-{message_id}.json",
        transcript_shard(ordinal)
    )
}

pub fn transcript_payload_dir(ordinal: u64, message_id: &str) -> String {
    format!(
        "{TRANSCRIPT_DIR}/{}/{ordinal:012}-{message_id}",
        transcript_shard(ordinal)
    )
}

pub fn request_record_path(request: &str) -> String {
    format!("{REQUESTS_DIR}/{request}.json")
}

pub fn tool_record_path(request: &str, round: u64, tool_id: &str) -> String {
    format!(
        "{TOOLS_DIR}/{request}/{round:04}/{}.json",
        admit_external_id(tool_id)
    )
}

pub fn tool_payload_dir(request: &str, round: u64, tool_id: &str) -> String {
    format!(
        "{TOOLS_DIR}/{request}/{round:04}/{}",
        admit_external_id(tool_id)
    )
}

pub fn async_record_path(task: &str) -> String {
    format!("{ASYNC_DIR}/{task}.json")
}

pub fn subagent_record_path(child: &str) -> String {
    format!("{SUBAGENTS_DIR}/{child}.json")
}

pub fn publication_record_path(publication: &str) -> String {
    format!("{PUBLICATIONS_DIR}/{publication}.json")
}

pub fn files_path(relative: &str) -> String {
    format!("{FILES_DIR}/{relative}")
}

pub fn parse_transcript_entry_path(path: &str) -> Result<(u64, String), String> {
    let invalid = || format!("invalid transcript entry path {path:?}");
    let rest = path
        .strip_prefix(&format!("{TRANSCRIPT_DIR}/"))
        .ok_or_else(invalid)?;
    let (shard, filename) = rest.split_once('/').ok_or_else(invalid)?;
    if shard.len() != 9 || !shard.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let stem = filename.strip_suffix(".json").ok_or_else(invalid)?;
    let (ordinal, message_id) = stem.split_once('-').ok_or_else(invalid)?;
    if ordinal.len() != 12
        || !ordinal.bytes().all(|byte| byte.is_ascii_digit())
        || validate_protocol_id_component(message_id).is_err()
    {
        return Err(invalid());
    }
    let ordinal = ordinal.parse::<u64>().map_err(|_| invalid())?;
    if transcript_shard(ordinal) != shard {
        return Err(invalid());
    }
    Ok((ordinal, message_id.to_string()))
}

pub fn parse_workspace_path(path: &str) -> Option<(String, WorkspaceFile)> {
    let rest = path.strip_prefix(&format!("{WORKSPACES_DIR}/"))?;
    let (name, filename) = rest.split_once('/')?;
    if filename.contains('/') || validate_workspace_name(name).is_err() {
        return None;
    }
    let kind = match filename {
        "commit" => WorkspaceFile::Commit,
        "initial" => WorkspaceFile::Initial,
        "origin" => WorkspaceFile::Origin,
        _ => return None,
    };
    Some((name.to_string(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_have_exact_layouts() {
        let message = "0123456789abcdef0123456789abcdef";
        assert_eq!(workspace_dir("main"), ".caos/workspaces/main");
        assert_eq!(
            workspace_commit_path("main"),
            ".caos/workspaces/main/commit"
        );
        assert_eq!(
            workspace_initial_path("main"),
            ".caos/workspaces/main/initial"
        );
        assert_eq!(
            workspace_origin_path("main"),
            ".caos/workspaces/main/origin"
        );
        assert_eq!(transcript_shard(1234), "000000001");
        assert_eq!(
            transcript_entry_path(1234, message),
            ".caos/transcript/000000001/000000001234-0123456789abcdef0123456789abcdef.json"
        );
        assert_eq!(
            transcript_payload_dir(1234, message),
            ".caos/transcript/000000001/000000001234-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(request_record_path("r"), ".caos/requests/r.json");
        assert_eq!(ACTIVE_REQUEST, ".caos/requests/active");
        assert_eq!(
            tool_record_path("r", 7, "toolu_01AB"),
            ".caos/tools/r/0007/toolu_01AB.json"
        );
        assert_eq!(
            tool_payload_dir("r", 7, "toolu_01AB"),
            ".caos/tools/r/0007/toolu_01AB"
        );
        assert_eq!(
            tool_record_path("r", 7, "weird id!"),
            ".caos/tools/r/0007/776569726420696421.json"
        );
        assert_eq!(async_record_path("task"), ".caos/async/task.json");
        assert_eq!(subagent_record_path("child"), ".caos/subagents/child.json");
        assert_eq!(
            publication_record_path("pub"),
            ".caos/publications/pub.json"
        );
        assert_eq!(files_path("a/b"), "files/a/b");
    }

    #[test]
    fn validators_enforce_portable_paths() {
        for valid in ["a", "a-b", "a_b", "é"] {
            assert!(validate_component(valid).is_ok(), "rejected {valid:?}");
        }
        for invalid in [
            "", ".", "..", ".git", ".GIT", "git~1", "a.", "a ", "a/b", "a\0b",
        ] {
            assert!(validate_component(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_component(&"a".repeat(256)).is_err());
        assert!(validate_tree_path("a/b/c").is_ok());
        assert!(validate_tree_path(&vec!["a"; 65].join("/")).is_err());
        for invalid in ["/a", "a/", "a//b", ""] {
            assert!(validate_tree_path(invalid).is_err());
        }
        assert!(validate_workspace_name("main-1.test").is_ok());
        assert!(validate_workspace_name(".hidden").is_err());
        assert!(validate_protocol_id_component("toolu_01AB").is_ok());
        assert!(validate_protocol_id_component("weird id!").is_err());
    }

    #[test]
    fn transcript_and_workspace_paths_parse() {
        let path = transcript_entry_path(1234, "message-1");
        assert_eq!(
            parse_transcript_entry_path(&path),
            Ok((1234, "message-1".to_string()))
        );
        assert!(parse_transcript_entry_path(
            ".caos/transcript/000000002/000000001234-message-1.json"
        )
        .is_err());
        assert_eq!(
            parse_workspace_path(".caos/workspaces/main/commit"),
            Some(("main".to_string(), WorkspaceFile::Commit))
        );
        assert_eq!(
            parse_workspace_path(".caos/workspaces/main/initial"),
            Some(("main".to_string(), WorkspaceFile::Initial))
        );
        assert_eq!(
            parse_workspace_path(".caos/workspaces/main/origin"),
            Some(("main".to_string(), WorkspaceFile::Origin))
        );
        assert_eq!(parse_workspace_path(".caos/workspaces/main/extra"), None);
    }
}
