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
    merge_base: &str,
    ours: &str,
    theirs: &str,
) -> Result<MergeTree, String> {
    for hash in [merge_base, ours, theirs] {
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

pub fn commit_tree(
    transport: &dyn Transport,
    tree: &str,
    parents: &[String],
    author: &str,
    committer: &str,
    message: &[u8],
) -> Result<String, String> {
    let bytes = commit_bytes(tree, parents, author, committer, message)?;
    // /object validates the tree and parent dependencies before publishing.
    transport
        .put_object("commit", &bytes)
        .map(|id| id.to_string())
}

fn commit_bytes(
    tree: &str,
    parents: &[String],
    author: &str,
    committer: &str,
    message: &[u8],
) -> Result<Vec<u8>, String> {
    let mut bytes = format!("tree {}\n", oid(tree)?);
    for parent in parents {
        bytes.push_str(&format!("parent {}\n", oid(parent)?));
    }
    for (kind, signature) in [("author", author), ("committer", committer)] {
        if signature.contains(['\n', '\r', '\0']) {
            return Err(format!("{kind} must be one Git signature"));
        }
        let mut remaining = signature.as_bytes();
        let parsed = gix::actor::SignatureRef::from_bytes_consuming(&mut remaining)
            .map_err(|e| format!("invalid {kind}: {e}"))?;
        if !remaining.is_empty() {
            return Err(format!("invalid {kind}: trailing signature data"));
        }
        parsed
            .time()
            .map_err(|e| format!("invalid {kind} timestamp: {e}"))?;
        bytes.push_str(&format!("{kind} {signature}\n"));
    }
    if message.contains(&0) {
        return Err("commit message contains NUL".into());
    }
    bytes.push('\n');
    let mut bytes = bytes.into_bytes();
    bytes.extend_from_slice(message);
    Ok(bytes)
}

fn oid(value: &str) -> Result<String, String> {
    if !git_locator::import::commit(value) {
        return Err("expected a full object hash".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn run_merge(args: &[String]) -> Result<(), String> {
    let [merge_base, ours, theirs] = args else {
        return Err("git-merge-tree requires <merge-base-tree> <ours-tree> <theirs-tree>".into());
    };
    let result = merge_tree(&crate::HttpTransport::from_env()?, merge_base, ours, theirs)?;
    println!(
        "{}",
        serde_json::json!({"tree":result.tree, "conflicts":result.conflicts})
    );
    Ok(())
}

pub fn run_commit(args: &[String]) -> Result<(), String> {
    let Some((tree, args)) = args.split_first() else {
        return Err("git-commit-tree requires <tree> --parent=<commit>... --author=<signature> --committer=<signature> --message-file=<path>".into());
    };
    let mut parents = Vec::new();
    let (mut author, mut committer, mut message_path) = (None, None, None);
    for arg in args {
        if let Some(parent) = arg.strip_prefix("--parent=") {
            parents.push(parent.to_owned());
        } else {
            let (name, value) = arg.split_once('=').ok_or("expected --name=value")?;
            let slot = match name {
                "--author" => &mut author,
                "--committer" => &mut committer,
                "--message-file" => &mut message_path,
                _ => return Err(format!("unknown git-commit-tree argument: {name}")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("duplicate {name}"));
            }
        }
    }
    let message = std::fs::read(message_path.ok_or("missing --message-file")?)
        .map_err(|e| format!("reading commit message: {e}"))?;
    let commit = commit_tree(
        &crate::HttpTransport::from_env()?,
        tree,
        &parents,
        author.ok_or("missing --author")?,
        committer.ok_or("missing --committer")?,
        &message,
    )?;
    println!("{commit}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const SIGNATURE: &str = "Example <example@example.com> 1700000000 +0530";

    #[test]
    fn commit_preserves_explicit_metadata_parent_order_and_message_bytes() {
        let tree = "a".repeat(40);
        let parents = vec!["b".repeat(40), "c".repeat(40)];
        let message = b"Subject\n\nBody without trailing newline";
        let bytes = commit_bytes(&tree, &parents, SIGNATURE, SIGNATURE, message).unwrap();
        let parsed = gix::objs::CommitRef::from_bytes(&bytes, gix::hash::Kind::Sha1).unwrap();
        assert_eq!(
            parsed.tree(),
            gix::ObjectId::from_hex(tree.as_bytes()).unwrap()
        );
        assert_eq!(
            parsed.parents().map(|p| p.to_string()).collect::<Vec<_>>(),
            parents
        );
        assert_eq!(parsed.message, message.as_slice());
        assert_eq!(
            commit_bytes(&tree, &parents, SIGNATURE, SIGNATURE, message).unwrap(),
            bytes
        );
        assert!(commit_bytes(&tree, &[], SIGNATURE, SIGNATURE, b"").is_ok());
    }

    #[test]
    fn preserves_non_utf8_commit_messages() {
        let message = b"Original message in Latin-1: caf\xe9\n";
        let bytes = commit_bytes(&"a".repeat(40), &[], SIGNATURE, SIGNATURE, message).unwrap();
        let parsed = gix::objs::CommitRef::from_bytes(&bytes, gix::hash::Kind::Sha1).unwrap();
        assert_eq!(parsed.message, message.as_slice());
    }

    #[test]
    fn rejects_invalid_ids_signatures_and_header_injection() {
        let tree = "a".repeat(40);
        assert!(commit_bytes("HEAD", &[], SIGNATURE, SIGNATURE, b"").is_err());
        assert!(commit_bytes(&tree, &["HEAD".into()], SIGNATURE, SIGNATURE, b"").is_err());
        for signature in [
            "Name <email>",
            "Name <email> yesterday +0000",
            "Name <email> 1 +0000\nparent ffffffffffffffffffffffffffffffffffffffff",
        ] {
            assert!(
                commit_bytes(&tree, &[], signature, SIGNATURE, b"").is_err(),
                "{signature}"
            );
        }
        assert!(commit_bytes(&tree, &[], SIGNATURE, SIGNATURE, b"bad\0message").is_err());
    }
}
