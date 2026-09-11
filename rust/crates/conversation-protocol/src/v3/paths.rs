use super::oid::hex_lower;

pub const FORMAT: &str = ".caos/format";
pub const FORMAT_BYTES: &str = "caos-conversation-v5\n";
pub const IDENTITY: &str = ".caos/identity.json";
pub const TITLE: &str = ".caos/title";
pub const TRANSCRIPT_DIR: &str = ".caos/transcript";
pub const CALLS_DIR: &str = ".caos/tools";
pub const CAOS_DIR: &str = ".caos";
pub const CONFLICTS_LEDGER: &str = ".caos/conflicts";

const MAX_PATH_COMPONENT: usize = 255;
pub const MAX_PATH: usize = 4096;
const MAX_TREE_DEPTH: usize = 64;
pub const MAX_TITLE: usize = 1024;
pub const MAX_JSON_INT: u64 = 9_007_199_254_740_991;

pub fn validate_format(bytes: &[u8]) -> Result<(), String> {
    match bytes {
        bytes if bytes == FORMAT_BYTES.as_bytes() => Ok(()),
        b"caos-conversation-v4\n" => Err("unsupported conversation format v4; this build reads v5. Open it with the earlier v4 build".into()),
        _ => Err("conversation format is invalid".into()),
    }
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

pub fn validate_source_tree_name(name: &str) -> Result<(), String> {
    validate_tree_path(name)?;
    if name == CAOS_DIR || name.starts_with(".caos/") {
        return Err("code references belong outside .caos".into());
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

pub fn source_tree_commit_path(name: &str) -> String {
    name.to_string()
}

pub fn transcript_entry_path(ordinal: u64, message_id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{ordinal:012}-{message_id}.json")
}

pub fn transcript_payload_dir(ordinal: u64, message_id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{ordinal:012}-{message_id}")
}

pub fn call_record_path(request: &str, round: u64, tool_id: &str) -> String {
    format!(
        "{CALLS_DIR}/{request}/{round:04}/{}.json",
        admit_external_id(tool_id)
    )
}

pub fn call_payload_dir(request: &str, round: u64, tool_id: &str) -> String {
    format!(
        "{CALLS_DIR}/{request}/{round:04}/{}",
        admit_external_id(tool_id)
    )
}

pub fn files_path(relative: &str) -> String {
    relative.to_string()
}

pub fn parse_transcript_entry_path(path: &str) -> Result<(u64, String), String> {
    let invalid = || format!("invalid transcript entry path {path:?}");
    let rest = path
        .strip_prefix(&format!("{TRANSCRIPT_DIR}/"))
        .ok_or_else(invalid)?;
    let filename = rest;
    let stem = filename.strip_suffix(".json").ok_or_else(invalid)?;
    let (ordinal, message_id) = stem.split_once('-').ok_or_else(invalid)?;
    if ordinal.len() != 12
        || !ordinal.bytes().all(|byte| byte.is_ascii_digit())
        || validate_protocol_id_component(message_id).is_err()
    {
        return Err(invalid());
    }
    let ordinal = ordinal.parse::<u64>().map_err(|_| invalid())?;
    Ok((ordinal, message_id.to_string()))
}
