//! Publish new source objects without packing an entire partial clone.
use conversation_protocol::v3::tree::{encode_commit_bytes, encode_tree_bytes};
use conversation_protocol::v3::{GitStore, Mode, ObjectStore, Oid};
use std::collections::HashSet;

/// Stop at objects already on the server. In particular, do not pack/push a
/// partial clone: Git could hydrate missing history just to form that pack.
pub(super) fn upload(
    store: &GitStore,
    server: &str,
    oid: &Oid,
    mode: Mode,
    seen: &mut HashSet<Oid>,
) -> Result<(), String> {
    if !seen.insert(oid.clone()) {
        return Ok(());
    }
    let endpoint = format!("{}/object/{oid}", server.trim_end_matches('/'));
    let response = minreq::head(&endpoint)
        .with_timeout(30)
        .send()
        .map_err(|e| e.to_string())?;
    match response.status_code {
        200..=299 => return Ok(()),
        404 => {}
        status => return Err(format!("object availability check returned {status}")),
    }
    let (kind, bytes) = match mode {
        Mode::Commit => {
            let commit = store.read_commit(oid)?;
            upload(store, server, &commit.tree, Mode::Tree, seen)?;
            for parent in &commit.parents {
                upload(store, server, parent, Mode::Commit, seen)?;
            }
            ("commit", encode_commit_bytes(&commit))
        }
        Mode::Tree => {
            let entries = store.read_tree(oid)?;
            for entry in &entries {
                // Gitlinks are independent histories, not children in this
                // ordinary tree closure. New draft commits are uploaded above.
                if entry.mode != Mode::Commit {
                    upload(store, server, &entry.oid, entry.mode, seen)?;
                }
            }
            ("tree", encode_tree_bytes(&entries))
        }
        _ => ("blob", store.read_blob(oid)?),
    };
    if kind != "tree" {
        // Keep the normal worker output-secret check for new blobs and commit
        // messages. Only individual changed objects are staged, never a tree.
        let directory = worker_common::scratch(&crate::fresh_name("upload-object"))?;
        let file = directory.join("object");
        std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
        let target = crate::fresh("uploaded-object");
        let command = if kind == "commit" {
            "put-commit"
        } else {
            "put"
        };
        worker_common::caos([command, worker_common::path(&file), &target])?;
        let published = worker_common::cas_hash(&target)?;
        if published != oid.as_str() {
            return Err("object upload returned a different hash".into());
        }
        return Ok(());
    }
    let mut body = format!("{kind} {}\0", bytes.len()).into_bytes();
    body.extend(bytes);
    let response = minreq::post(format!("{}/object/", server.trim_end_matches('/')))
        .with_body(body)
        .with_timeout(30)
        .send()
        .map_err(|e| e.to_string())?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!("object upload returned {}", response.status_code));
    }
    if response.as_str().map(str::trim).ok() != Some(oid.as_str()) {
        return Err("object upload returned a different hash".into());
    }
    Ok(())
}
