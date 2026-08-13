//! Append durable events to a conversation's one authoritative ref.
//!
//! The worker image has no `git`, so this module uses the server's object API
//! and the small part of smart-HTTP receive-pack needed for a compare-and-swap
//! ref update. Event objects are stored before the ref moves. A successful
//! return therefore means the event is reachable from
//! `refs/caos/conversations/<id>/head`; failures are never downgraded to
//! observability warnings.

use serde_json::Value;

/// The empty SHA-1 packfile: header (`PACK`, version 2, zero objects) and its
/// checksum. Event commits and their trees have already been uploaded through
/// `/object`, so receive-pack only has to perform the ref transaction.
const EMPTY_PACK: &[u8] = &[
    b'P', b'A', b'C', b'K', 0, 0, 0, 2, 0, 0, 0, 0, // header
    0x02, 0x9d, 0x08, 0x82, 0x3b, 0xd8, 0xa8, 0xea, 0xb5, 0x10, // sha1…
    0xad, 0x6a, 0xc7, 0x5c, 0x82, 0x3c, 0xfd, 0x3e, 0xd3, 0x1e,
];

const MAX_CAS_ATTEMPTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResult {
    /// The event commit now reachable from the conversation ref.
    pub commit: String,
    /// The head this event commit directly follows on the first-parent spine.
    pub previous_head: String,
    /// Number of compare-and-swap races lost before the successful append.
    pub retries: usize,
}

#[derive(Debug, Clone)]
struct RemoteCommit {
    tree: String,
    parents: Vec<String>,
    message: String,
}

/// One version-2 event on the canonical first-parent conversation spine.
#[derive(Debug, Clone)]
pub struct ConversationEvent {
    pub commit: String,
    pub tree: String,
    pub value: Value,
}

/// A chronological snapshot of the canonical conversation event log.
#[derive(Debug, Clone)]
pub struct ConversationLog {
    pub head: String,
    pub events: Vec<ConversationEvent>,
}

enum PushResult {
    Updated,
    Rejected(String),
}

/// Append one JSON event to `refs/caos/conversations/<conversation>/head`.
///
/// `tree` is the already-uploaded workspace tree for the proposed event.
/// `extra_parent` keeps a workspace mutation/merge commit reachable from the
/// event; it is omitted when it equals the current conversation head.
///
/// On a CAS race, a tree-neutral side can be replayed over the other side. If
/// both the concurrent event and this proposal changed the original base tree
/// to different trees, this returns a conflict instead of choosing or
/// attempting an implicit merge.
pub fn append_event(
    conversation: &str,
    event: &Value,
    tree: &str,
    extra_parent: Option<&str>,
) -> Result<AppendResult, String> {
    let refname = conversation_ref(conversation)?;
    validate_hash(tree, "event tree")?;
    if let Some(parent) = extra_parent {
        validate_hash(parent, "extra parent")?;
    }
    if !event.is_object() {
        return Err("conversation event must be a JSON object".to_string());
    }
    let message =
        serde_json::to_string(event).map_err(|e| format!("serializing conversation event: {e}"))?;
    let base = server_base()?;

    // llm-step only runs after the client has appended the user event, so an
    // absent head is a broken dispatch rather than an invitation to create a
    // root with unknowable ancestry.
    let original_head = advertised(&base, &refname)?
        .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
    let original_base_tree = fetch_commit(&base, &original_head)?.tree;

    let mut expected = original_head;
    let mut expected_tree = original_base_tree;
    let mut candidate_parent: Option<String> = None;
    let mut candidate_tree = tree.to_string();

    for retries in 0..MAX_CAS_ATTEMPTS {
        let parents = if let Some(stale_candidate) = candidate_parent.as_deref() {
            vec![expected.as_str(), stale_candidate]
        } else {
            let mut parents = vec![expected.as_str()];
            if let Some(parent) = extra_parent {
                if parent != expected {
                    parents.push(parent);
                }
            }
            parents
        };
        let candidate = store_commit(&base, &candidate_tree, &parents, &message)?;

        match push_ref(&base, &refname, &expected, &candidate) {
            Ok(PushResult::Updated) => {
                return Ok(AppendResult {
                    commit: candidate,
                    previous_head: expected,
                    retries,
                })
            }
            // A dropped response can hide an accepted push. Read the ref before
            // returning the transport error so retrying the worker cannot
            // append the same candidate twice at the same head.
            Err(error) => {
                let observed = advertised(&base, &refname).map_err(|read_error| {
                    format!(
                        "pushing {refname} failed ({error}); rereading it also failed: {read_error}"
                    )
                })?;
                if observed.as_deref() == Some(candidate.as_str()) {
                    return Ok(AppendResult {
                        commit: candidate,
                        previous_head: expected,
                        retries,
                    });
                }
                if observed.as_deref() == Some(expected.as_str()) {
                    return Err(error);
                }
                // A different head means another append won while our request
                // was in flight. Handle it through the normal race path.
            }
            Ok(PushResult::Rejected(report)) => {
                let observed = advertised(&base, &refname)?;
                if observed.as_deref() == Some(candidate.as_str()) {
                    return Ok(AppendResult {
                        commit: candidate,
                        previous_head: expected,
                        retries,
                    });
                }
                if observed.as_deref() == Some(expected.as_str()) {
                    return Err(format!(
                        "server rejected update of {refname} without a CAS race: {report}"
                    ));
                }
            }
        }

        let current = advertised(&base, &refname)?
            .ok_or_else(|| format!("conversation ref {refname} disappeared during append"))?;
        let current_tree = fetch_commit(&base, &current)?.tree;
        candidate_tree = retry_tree(&expected_tree, &candidate_tree, &current_tree)?;
        candidate_parent = Some(candidate);
        expected = current;
        expected_tree = current_tree;
    }

    Err(format!(
        "conversation ref {refname} kept changing after {MAX_CAS_ATTEMPTS} append attempts"
    ))
}

/// Read all version-2 events reachable from the canonical conversation head,
/// following only the first parent. The first non-event commit is the
/// workspace base and is deliberately excluded.
pub fn conversation_log(conversation: &str) -> Result<ConversationLog, String> {
    let refname = conversation_ref(conversation)?;
    let base = server_base()?;
    let head = advertised(&base, &refname)?
        .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
    let mut newest_first = Vec::new();
    let mut current = head.clone();

    loop {
        let commit = fetch_commit(&base, &current)?;
        let Ok(value) = serde_json::from_str::<Value>(commit.message.trim()) else {
            break;
        };
        if value.get("v").and_then(Value::as_u64) != Some(2) || !value.is_object() {
            break;
        }
        let parent = commit.parents.first().cloned();
        newest_first.push(ConversationEvent {
            commit: current,
            tree: commit.tree,
            value,
        });
        let Some(parent) = parent else {
            break;
        };
        current = parent;
    }
    newest_first.reverse();
    Ok(ConversationLog {
        head,
        events: newest_first,
    })
}

fn conversation_ref(conversation: &str) -> Result<String, String> {
    validate_conversation(conversation)?;
    Ok(format!("refs/caos/conversations/{conversation}/head"))
}

/// A conservative in-worker equivalent of `git check-ref-format` for the id
/// portion. In particular, `head` is reserved as *any* path component: allowing
/// `a/head/b` would make its ref collide with conversation `a`'s ref-as-file.
fn validate_conversation(conversation: &str) -> Result<(), String> {
    if conversation.is_empty() || conversation.len() > 512 {
        return Err(format!("invalid conversation name {conversation:?}"));
    }
    if conversation.starts_with('/')
        || conversation.ends_with('/')
        || conversation.contains("//")
        || conversation.contains("..")
        || conversation.contains("@{")
        || conversation.ends_with('.')
        || conversation.bytes().any(|b| {
            b <= b' ' || b == 0x7f || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return Err(format!("invalid conversation name {conversation:?}"));
    }
    for component in conversation.split('/') {
        if component.is_empty()
            || component == "."
            || component == "head"
            || component.starts_with('.')
            || component.ends_with(".lock")
        {
            return Err(format!("invalid conversation name {conversation:?}"));
        }
    }
    Ok(())
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

fn retry_tree(base: &str, proposed: &str, current: &str) -> Result<String, String> {
    if current == base {
        Ok(proposed.to_string())
    } else if proposed == base || proposed == current {
        Ok(current.to_string())
    } else {
        Err(format!(
            "conversation workspace conflict: base tree {base}, proposed tree {proposed}, concurrent tree {current}"
        ))
    }
}

fn store_commit(base: &str, tree: &str, parents: &[&str], message: &str) -> Result<String, String> {
    let content = commit_content(tree, parents, message)?;
    store_object(base, "commit", content.as_bytes())
}

fn commit_content(tree: &str, parents: &[&str], message: &str) -> Result<String, String> {
    validate_hash(tree, "commit tree")?;
    if parents.is_empty() {
        return Err("conversation event commit requires a parent".to_string());
    }
    let mut content = format!("tree {tree}\n");
    for parent in parents {
        validate_hash(parent, "commit parent")?;
        content.push_str(&format!("parent {parent}\n"));
    }
    // A fixed identity makes a proposal stable for a given event, tree, and
    // observed head. The event itself may carry a wall-clock timestamp when it
    // is useful to the transcript.
    content.push_str(
        "author caos-agent <caos@caos> 0 +0000\n\
         committer caos-agent <caos@caos> 0 +0000\n\n",
    );
    content.push_str(message);
    if !message.ends_with('\n') {
        content.push('\n');
    }
    Ok(content)
}

fn store_object(base: &str, kind: &str, content: &[u8]) -> Result<String, String> {
    let mut body = format!("{kind} {}\0", content.len()).into_bytes();
    body.extend_from_slice(content);
    let url = format!("{base}/object/");
    let resp = minreq::post(&url)
        .with_body(body)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let hash = resp
        .as_str()
        .map_err(|e| format!("POST {url}: invalid response: {e}"))?
        .trim()
        .to_string();
    validate_hash(&hash, "stored object")?;
    Ok(hash)
}

fn fetch_commit(base: &str, hash: &str) -> Result<RemoteCommit, String> {
    validate_hash(hash, "commit")?;
    let url = format!("{base}/object/{hash}");
    let resp = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    parse_serialized_commit(resp.as_bytes())
        .map_err(|e| format!("GET {url}: invalid commit object: {e}"))
}

fn parse_serialized_commit(serialized: &[u8]) -> Result<RemoteCommit, String> {
    let nul = serialized
        .iter()
        .position(|b| *b == 0)
        .ok_or("object has no header terminator")?;
    let header = std::str::from_utf8(&serialized[..nul])
        .map_err(|e| format!("object header is not UTF-8: {e}"))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| format!("malformed object header {header:?}"))?;
    if kind != "commit" {
        return Err(format!("expected commit, got {kind:?}"));
    }
    let declared: usize = size
        .parse()
        .map_err(|e| format!("invalid object size {size:?}: {e}"))?;
    let content = &serialized[nul + 1..];
    if declared != content.len() {
        return Err(format!(
            "declared size {declared} does not match {} bytes",
            content.len()
        ));
    }
    let text =
        std::str::from_utf8(content).map_err(|e| format!("commit content is not UTF-8: {e}"))?;
    let (headers, message) = text
        .split_once("\n\n")
        .ok_or("commit has no header/message separator")?;
    let tree = headers
        .lines()
        .find_map(|line| line.strip_prefix("tree "))
        .ok_or("commit has no tree")?
        .to_string();
    validate_hash(&tree, "commit tree")?;
    let mut parents = Vec::new();
    for line in headers.lines() {
        if let Some(parent) = line.strip_prefix("parent ") {
            validate_hash(parent, "commit parent")?;
            parents.push(parent.to_string());
        }
    }
    Ok(RemoteCommit {
        tree,
        parents,
        message: message.to_string(),
    })
}

fn server_base() -> Result<String, String> {
    let base =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    Ok(base.trim_end_matches('/').to_string())
}

fn push_ref(
    base: &str,
    refname: &str,
    expected: &str,
    new_hash: &str,
) -> Result<PushResult, String> {
    validate_hash(expected, "expected ref")?;
    validate_hash(new_hash, "new ref")?;

    let command = format!("{expected} {new_hash} {refname}\0report-status");
    let mut body = pkt_line(command.as_bytes())?;
    body.extend_from_slice(b"0000");
    body.extend_from_slice(EMPTY_PACK);

    let url = format!("{base}/git-receive-pack");
    let resp = minreq::post(&url)
        .with_header("content-type", "application/x-git-receive-pack-request")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    parse_push_report(resp.as_bytes(), refname)
}

fn parse_push_report(report: &[u8], refname: &str) -> Result<PushResult, String> {
    let lines = decode_pkt_lines(report)?;
    let mut unpack_ok = false;
    let mut ref_ok = false;
    let mut rejection = None;
    for line in lines {
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

/// The hash currently advertised for `refname`, or `None` when it is absent.
fn advertised(base: &str, refname: &str) -> Result<Option<String>, String> {
    let url = format!("{base}/info/refs?service=git-receive-pack");
    let resp = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    for payload in decode_pkt_lines(resp.as_bytes())? {
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn validates_conversation_ids_and_reserves_head_components() {
        assert_eq!(
            conversation_ref("project/talk-1").unwrap(),
            "refs/caos/conversations/project/talk-1/head"
        );
        for invalid in [
            "", "bad name", "a//b", "a/../b", ".hidden", "a.lock", "head", "a/head/b", "a~b",
            "a@{b",
        ] {
            assert!(
                conversation_ref(invalid).is_err(),
                "accepted invalid id {invalid:?}"
            );
        }
    }

    #[test]
    fn retry_tree_only_replays_a_tree_neutral_side() {
        assert_eq!(retry_tree(A, B, A).unwrap(), B);
        assert_eq!(retry_tree(A, A, C).unwrap(), C);
        assert_eq!(retry_tree(A, B, B).unwrap(), B);
        assert!(retry_tree(A, B, C).is_err());
    }

    #[test]
    fn parses_git_serialized_commit() {
        let content = format!(
            "tree {A}\nparent {B}\nauthor x <x@x> 0 +0000\ncommitter x <x@x> 0 +0000\n\n{{\"author\":\"assistant\"}}\n"
        );
        let mut object = format!("commit {}\0", content.len()).into_bytes();
        object.extend_from_slice(content.as_bytes());
        let parsed = parse_serialized_commit(&object).unwrap();
        assert_eq!(parsed.tree, A);
    }

    #[test]
    fn event_commit_has_ordered_parents_and_stable_identity() {
        let content = commit_content(A, &[B, C], r#"{"author":"assistant","content":"hi"}"#)
            .unwrap();
        assert_eq!(
            content,
            format!(
                "tree {A}\nparent {B}\nparent {C}\nauthor caos-agent <caos@caos> 0 +0000\ncommitter caos-agent <caos@caos> 0 +0000\n\n{{\"author\":\"assistant\",\"content\":\"hi\"}}\n"
            )
        );
    }

    #[test]
    fn pkt_lines_round_trip_and_accept_flushes() {
        let one = pkt_line(b"unpack ok\n").unwrap();
        let two = pkt_line(b"ok refs/caos/conversations/talk-1/head\n").unwrap();
        let mut wire = one;
        wire.extend_from_slice(b"0000");
        wire.extend_from_slice(&two);
        wire.extend_from_slice(b"0000");
        let decoded = decode_pkt_lines(&wire).unwrap();
        assert_eq!(
            decoded,
            vec![
                b"unpack ok\n".as_slice(),
                b"ok refs/caos/conversations/talk-1/head\n".as_slice()
            ]
        );
    }

    #[test]
    fn push_report_distinguishes_success_and_rejection() {
        let refname = "refs/caos/conversations/talk-1/head";
        let mut ok = pkt_line(b"unpack ok\n").unwrap();
        ok.extend_from_slice(&pkt_line(format!("ok {refname}\n").as_bytes()).unwrap());
        assert!(matches!(
            parse_push_report(&ok, refname).unwrap(),
            PushResult::Updated
        ));

        let mut rejected = pkt_line(b"unpack ok\n").unwrap();
        rejected
            .extend_from_slice(&pkt_line(format!("ng {refname} stale info\n").as_bytes()).unwrap());
        match parse_push_report(&rejected, refname).unwrap() {
            PushResult::Rejected(reason) => assert_eq!(reason, "stale info"),
            PushResult::Updated => panic!("rejection was accepted"),
        }
    }

    #[test]
    fn invalid_object_size_is_rejected() {
        let object = format!("commit 1\0tree {A}\n\nmsg\n");
        assert!(parse_serialized_commit(object.as_bytes()).is_err());
    }
}
