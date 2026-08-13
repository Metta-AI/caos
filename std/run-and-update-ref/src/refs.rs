//! Minimal status-only event append protocol for the finish stage.
//!
//! This intentionally mirrors llm-step's receive-pack client locally: std
//! workers are built as standalone projects and cannot import a module from
//! another std binary. Status appends are tree-neutral, so a CAS loser simply
//! retries from the latest head with that head's tree.

use std::cmp::Ordering;
use std::collections::BTreeMap;

type TreeEntries = BTreeMap<Vec<u8>, (u32, String)>;

const EMPTY_PACK: &[u8] = &[
    b'P', b'A', b'C', b'K', 0, 0, 0, 2, 0, 0, 0, 0, // header
    0x02, 0x9d, 0x08, 0x82, 0x3b, 0xd8, 0xa8, 0xea, 0xb5, 0x10, 0xad, 0x6a, 0xc7, 0x5c, 0x82, 0x3c,
    0xfd, 0x3e, 0xd3, 0x1e,
];
const MAX_CAS_ATTEMPTS: usize = 32;

enum PushResult {
    Updated,
    Rejected(String),
}

pub fn validate_target_ref(refname: &str) -> Result<(), String> {
    let Some(rest) = refname.strip_prefix("refs/caos/conversations/") else {
        return Err(format!(
            "target-ref is not a conversation head: {refname:?}"
        ));
    };
    let Some(conversation) = rest.strip_suffix("/head") else {
        return Err(format!(
            "target-ref is not a conversation head: {refname:?}"
        ));
    };
    if conversation.is_empty()
        || conversation.len() > 512
        || conversation.starts_with('/')
        || conversation.ends_with('/')
        || conversation.contains("//")
        || conversation.contains("..")
        || conversation.contains("@{")
        || conversation.ends_with('.')
        || conversation.bytes().any(|b| {
            b <= b' ' || b == 0x7f || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        || conversation.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || component == "head"
                || component.starts_with('.')
                || component.ends_with(".lock")
        })
    {
        return Err(format!("invalid target conversation ref {refname:?}"));
    }
    Ok(())
}

pub fn append_status(refname: &str, task: &str, status: &str) -> Result<(), String> {
    validate_target_ref(refname)?;
    validate_hash(task, "task")?;
    if !matches!(status, "complete" | "failed") {
        return Err(format!("invalid async status {status:?}"));
    }
    let base = server_base()?;

    for _ in 0..MAX_CAS_ATTEMPTS {
        let head = advertised(&base, refname)?
            .ok_or_else(|| format!("target conversation ref {refname} does not exist"))?;
        let remote = fetch_commit(&base, &head)?;
        if let Some(current) = task_status(&base, &remote.tree, task)? {
            if !should_append(&current, status) {
                return Ok(());
            }
        }

        let tree = status_tree(&base, &remote.tree, task, status)?;
        let message = "{\"v\":2}\n";
        let commit = store_commit(&base, &tree, &head, &message)?;
        match push_ref(&base, refname, &head, &commit) {
            Ok(PushResult::Updated) => return Ok(()),
            Ok(PushResult::Rejected(report)) => {
                let observed = advertised(&base, refname)?;
                if observed.as_deref() == Some(commit.as_str()) {
                    return Ok(());
                }
                if observed.as_deref() == Some(head.as_str()) {
                    return Err(format!(
                        "server rejected update of {refname} without a CAS race: {report}"
                    ));
                }
            }
            Err(error) => {
                let observed = advertised(&base, refname).map_err(|read_error| {
                    format!(
                        "pushing {refname} failed ({error}); rereading it also failed: {read_error}"
                    )
                })?;
                if observed.as_deref() == Some(commit.as_str()) {
                    return Ok(());
                }
                if observed.as_deref() == Some(head.as_str()) {
                    return Err(error);
                }
            }
        }
    }
    Err(format!(
        "target ref {refname} kept changing after {MAX_CAS_ATTEMPTS} attempts"
    ))
}

fn should_append(current: &str, desired: &str) -> bool {
    let current = current.trim();
    current != "canceled" && current != desired
}

fn task_status(base: &str, root: &str, task: &str) -> Result<Option<String>, String> {
    let Some(caos) = tree_entry(base, root, b".caos")? else {
        return Ok(None);
    };
    let Some(async_tree) = tree_entry(base, &caos, b"async")? else {
        return Ok(None);
    };
    let Some(task_tree) = tree_entry(base, &async_tree, task.as_bytes())? else {
        return Ok(None);
    };
    let Some(status) = tree_entry(base, &task_tree, b"status")? else {
        return Ok(None);
    };
    fetch_blob(base, &status).map(Some)
}

fn status_tree(base: &str, root: &str, task: &str, status: &str) -> Result<String, String> {
    let status_blob = store_object(base, "blob", status.as_bytes())?;
    let existing_caos = tree_entry(base, root, b".caos")?;
    let existing_async = match existing_caos.as_deref() {
        Some(caos) => tree_entry(base, caos, b"async")?,
        None => None,
    };
    let existing_task = match existing_async.as_deref() {
        Some(async_tree) => tree_entry(base, async_tree, task.as_bytes())?,
        None => None,
    };
    let task_tree = upsert_tree(
        base,
        existing_task.as_deref(),
        b"status",
        0o100644,
        &status_blob,
    )?;
    let async_tree = upsert_tree(
        base,
        existing_async.as_deref(),
        task.as_bytes(),
        0o040000,
        &task_tree,
    )?;
    let caos_tree = upsert_tree(
        base,
        existing_caos.as_deref(),
        b"async",
        0o040000,
        &async_tree,
    )?;
    upsert_tree(base, Some(root), b".caos", 0o040000, &caos_tree)
}

fn upsert_tree(
    base: &str,
    tree: Option<&str>,
    name: &[u8],
    mode: u32,
    oid: &str,
) -> Result<String, String> {
    let mut entries = match tree {
        Some(tree) => fetch_tree(base, tree)?,
        None => BTreeMap::new(),
    };
    entries.insert(name.to_vec(), (mode, oid.to_string()));
    store_tree(base, &entries)
}

fn tree_entry(base: &str, tree: &str, name: &[u8]) -> Result<Option<String>, String> {
    Ok(fetch_tree(base, tree)?.remove(name).map(|(_, oid)| oid))
}

fn fetch_tree(base: &str, hash: &str) -> Result<TreeEntries, String> {
    let (kind, content) = get_object(base, hash)?;
    if kind != "tree" {
        return Err(format!("object {hash} is a {kind}, not a tree"));
    }
    parse_tree(&content)
}

fn parse_tree(mut content: &[u8]) -> Result<TreeEntries, String> {
    let mut entries = BTreeMap::new();
    while !content.is_empty() {
        let space = content
            .iter()
            .position(|b| *b == b' ')
            .ok_or("tree entry has no mode")?;
        let nul = content
            .iter()
            .position(|b| *b == 0)
            .ok_or("tree entry has no NUL")?;
        if nul <= space || content.len() < nul + 21 {
            return Err("malformed tree entry".to_string());
        }
        let mode = std::str::from_utf8(&content[..space])
            .map_err(|e| format!("tree mode is not UTF-8: {e}"))?;
        let mode = u32::from_str_radix(mode, 8).map_err(|e| format!("invalid tree mode: {e}"))?;
        if !matches!(mode, 0o040000 | 0o100644 | 0o100755 | 0o120000 | 0o160000) {
            return Err(format!("unsupported tree mode {mode:o}"));
        }
        let name = content[space + 1..nul].to_vec();
        if name.is_empty() || name.contains(&b'/') {
            return Err("invalid tree entry name".to_string());
        }
        if entries
            .insert(name, (mode, hex(&content[nul + 1..nul + 21])))
            .is_some()
        {
            return Err("tree has duplicate entry names".to_string());
        }
        content = &content[nul + 21..];
    }
    Ok(entries)
}

fn store_tree(base: &str, entries: &TreeEntries) -> Result<String, String> {
    store_object(base, "tree", &encode_tree(entries)?)
}

fn encode_tree(entries: &TreeEntries) -> Result<Vec<u8>, String> {
    let mut entries: Vec<_> = entries.iter().collect();
    entries.sort_by(
        |(left_name, (left_mode, _)), (right_name, (right_mode, _))| {
            git_tree_order(left_name, *left_mode, right_name, *right_mode)
        },
    );
    let mut content = Vec::new();
    for (name, (mode, oid)) in entries {
        content.extend_from_slice(format!("{mode:o} ").as_bytes());
        content.extend_from_slice(name);
        content.push(0);
        content.extend_from_slice(&unhex(oid)?);
    }
    Ok(content)
}

// Git compares a directory name as though it ended in `/`, rather than at the
// end of the byte string. This differs from ordinary byte sorting for e.g. a
// directory `foo` and file `foo.bar`.
fn git_tree_order(left: &[u8], left_mode: u32, right: &[u8], right_mode: u32) -> Ordering {
    let shared = left.len().min(right.len());
    match left[..shared].cmp(&right[..shared]) {
        Ordering::Equal => {
            let left_next = left
                .get(shared)
                .copied()
                .unwrap_or(if left_mode == 0o040000 { b'/' } else { 0 });
            let right_next = right
                .get(shared)
                .copied()
                .unwrap_or(if right_mode == 0o040000 { b'/' } else { 0 });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

struct RemoteCommit {
    tree: String,
}

fn fetch_commit(base: &str, hash: &str) -> Result<RemoteCommit, String> {
    let (kind, content) = get_object(base, hash)?;
    if kind != "commit" {
        return Err(format!("object {hash} is a {kind}, not a commit"));
    }
    let text = std::str::from_utf8(&content).map_err(|e| format!("commit is not UTF-8: {e}"))?;
    let (headers, _) = text
        .split_once("\n\n")
        .ok_or("commit has no header/message separator")?;
    let tree = headers
        .lines()
        .find_map(|line| line.strip_prefix("tree "))
        .ok_or("commit has no tree")?
        .to_string();
    validate_hash(&tree, "commit tree")?;
    Ok(RemoteCommit { tree })
}

fn store_commit(base: &str, tree: &str, parent: &str, message: &str) -> Result<String, String> {
    validate_hash(tree, "commit tree")?;
    validate_hash(parent, "commit parent")?;
    let content = format!(
        "tree {tree}\nparent {parent}\nauthor caos-async <caos@caos> 0 +0000\n\
         committer caos-async <caos@caos> 0 +0000\n\n{message}"
    );
    store_object(base, "commit", content.as_bytes())
}

fn fetch_blob(base: &str, hash: &str) -> Result<String, String> {
    let (kind, content) = get_object(base, hash)?;
    if kind != "blob" {
        return Err(format!("object {hash} is a {kind}, not a blob"));
    }
    String::from_utf8(content).map_err(|e| format!("blob {hash} is not UTF-8: {e}"))
}

fn get_object(base: &str, hash: &str) -> Result<(String, Vec<u8>), String> {
    validate_hash(hash, "object")?;
    let url = format!("{base}/object/{hash}");
    let response = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    parse_object(response.as_bytes())
}

fn parse_object(serialized: &[u8]) -> Result<(String, Vec<u8>), String> {
    let nul = serialized
        .iter()
        .position(|b| *b == 0)
        .ok_or("object has no NUL")?;
    let header = std::str::from_utf8(&serialized[..nul])
        .map_err(|e| format!("object header is not UTF-8: {e}"))?;
    let (kind, size) = header.split_once(' ').ok_or("malformed object header")?;
    let size: usize = size
        .parse()
        .map_err(|e| format!("invalid object size: {e}"))?;
    let content = &serialized[nul + 1..];
    if content.len() != size {
        return Err(format!(
            "object size {size} != content length {}",
            content.len()
        ));
    }
    Ok((kind.to_string(), content.to_vec()))
}

fn store_object(base: &str, kind: &str, content: &[u8]) -> Result<String, String> {
    let mut body = format!("{kind} {}\0", content.len()).into_bytes();
    body.extend_from_slice(content);
    let url = format!("{base}/object/");
    let response = minreq::post(&url)
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let hash = response
        .as_str()
        .map_err(|e| format!("POST {url}: {e}"))?
        .trim();
    validate_hash(hash, "stored object")?;
    Ok(hash.to_string())
}

fn server_base() -> Result<String, String> {
    std::env::var("CAOS_SERVER_URL")
        .map(|base| base.trim_end_matches('/').to_string())
        .map_err(|_| "CAOS_SERVER_URL not set".to_string())
}

fn push_ref(base: &str, refname: &str, old: &str, new: &str) -> Result<PushResult, String> {
    validate_hash(old, "expected ref")?;
    validate_hash(new, "new ref")?;
    let command = format!("{old} {new} {refname}\0report-status");
    let mut body = pkt_line(command.as_bytes())?;
    body.extend_from_slice(b"0000");
    body.extend_from_slice(EMPTY_PACK);
    let url = format!("{base}/git-receive-pack");
    let response = minreq::post(&url)
        .with_header("content-type", "application/x-git-receive-pack-request")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    parse_push_report(response.as_bytes(), refname)
}

fn advertised(base: &str, refname: &str) -> Result<Option<String>, String> {
    let url = format!("{base}/info/refs?service=git-receive-pack");
    let response = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    for payload in decode_pkt_lines(response.as_bytes())? {
        let record = payload.split(|b| *b == 0).next().unwrap_or(payload);
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        let Some(space) = record.iter().position(|b| *b == b' ') else {
            continue;
        };
        let hash = std::str::from_utf8(&record[..space]).unwrap_or("");
        let name = std::str::from_utf8(&record[space + 1..]).unwrap_or("");
        if name == refname {
            validate_hash(hash, "advertised ref")?;
            return Ok(Some(hash.to_string()));
        }
    }
    Ok(None)
}

fn parse_push_report(report: &[u8], refname: &str) -> Result<PushResult, String> {
    let mut unpack_ok = false;
    let mut ref_ok = false;
    let mut rejection = None;
    for line in decode_pkt_lines(report)? {
        let line = std::str::from_utf8(line)
            .map_err(|e| format!("receive-pack report is not UTF-8: {e}"))?
            .trim_end_matches('\n');
        if line == "unpack ok" {
            unpack_ok = true;
        } else if line == format!("ok {refname}") {
            ref_ok = true;
        } else if let Some(reason) = line.strip_prefix(&format!("ng {refname} ")) {
            rejection = Some(reason.to_string());
        } else if let Some(reason) = line.strip_prefix("unpack ") {
            rejection = Some(reason.to_string());
        }
    }
    if unpack_ok && ref_ok {
        Ok(PushResult::Updated)
    } else if let Some(reason) = rejection {
        Ok(PushResult::Rejected(reason))
    } else {
        Err(format!(
            "receive-pack did not acknowledge {refname}: {}",
            String::from_utf8_lossy(report).trim()
        ))
    }
}

fn pkt_line(payload: &[u8]) -> Result<Vec<u8>, String> {
    let len = payload.len() + 4;
    if len > 0xffff {
        return Err(format!(
            "pkt-line payload is too long: {} bytes",
            payload.len()
        ));
    }
    let mut out = format!("{len:04x}").into_bytes();
    out.extend_from_slice(payload);
    Ok(out)
}

fn decode_pkt_lines(mut input: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut lines = Vec::new();
    while !input.is_empty() {
        if input.len() < 4 {
            return Err("truncated pkt-line length".to_string());
        }
        let header = std::str::from_utf8(&input[..4])
            .map_err(|e| format!("pkt-line length is not ASCII: {e}"))?;
        let len = usize::from_str_radix(header, 16)
            .map_err(|e| format!("invalid pkt-line length {header:?}: {e}"))?;
        input = &input[4..];
        if len == 0 || len == 1 || len == 2 {
            continue;
        }
        if len < 4 || input.len() < len - 4 {
            return Err(format!("truncated pkt-line body of length {len}"));
        }
        let payload_len = len - 4;
        lines.push(&input[..payload_len]);
        input = &input[payload_len..];
    }
    Ok(lines)
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    validate_hash(text, "object")?;
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| format!("invalid hex: {e}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ref_is_only_a_conversation_head() {
        assert!(validate_target_ref("refs/caos/conversations/chat-1/head").is_ok());
        assert!(validate_target_ref("refs/heads/main").is_err());
        assert!(validate_target_ref("refs/caos/conversations/chat-1/status").is_err());
        assert!(validate_target_ref("refs/caos/conversations/a/head/b/head").is_err());
    }

    #[test]
    fn tree_codec_round_trips_and_sorts() {
        let mut entries = BTreeMap::new();
        entries.insert(b"z".to_vec(), (0o100644, "a".repeat(40)));
        entries.insert(b"a".to_vec(), (0o040000, "b".repeat(40)));
        let encoded = encode_tree(&entries).unwrap();
        assert_eq!(parse_tree(&encoded).unwrap(), entries);
    }

    #[test]
    fn tree_encoding_uses_git_directory_order() {
        let mut entries = BTreeMap::new();
        entries.insert(b"foo".to_vec(), (0o040000, "a".repeat(40)));
        entries.insert(b"foo.bar".to_vec(), (0o100644, "b".repeat(40)));
        let encoded = encode_tree(&entries).unwrap();
        let first_nul = encoded.iter().position(|byte| *byte == 0).unwrap();
        assert_eq!(&encoded[..first_nul], b"100644 foo.bar");
    }

    #[test]
    fn canceled_and_identical_statuses_are_not_replaced() {
        assert!(!should_append("canceled", "complete"));
        assert!(!should_append("complete\n", "complete"));
        assert!(should_append("pending", "complete"));
        assert!(should_append("failed", "complete"));
    }

    #[test]
    fn packet_lines_round_trip() {
        let mut wire = pkt_line(b"unpack ok\n").unwrap();
        wire.extend_from_slice(b"0000");
        wire.extend_from_slice(&pkt_line(b"ok refs/caos/conversations/a/head\n").unwrap());
        assert_eq!(
            decode_pkt_lines(&wire).unwrap(),
            vec![
                b"unpack ok\n".as_slice(),
                b"ok refs/caos/conversations/a/head\n".as_slice()
            ]
        );
    }
}
