//! Storage: the git object database behind `/object`.
//!
//! Objects cross the wire in git's native serialized form,
//! `<type> <size>\0<content>` (uncompressed). The same in-process `gix` repo also
//! backs the compute half, which reads trees/blobs directly via [`fetch_tree`] /
//! [`fetch_blob`] (no HTTP round-trip).

use crate::{Config, HttpError};

/// A git tree entry, owned so it outlives the fetched object bytes.
pub(crate) struct TreeEntry {
    pub(crate) name: String,
    pub(crate) mode: gix::objs::tree::EntryMode,
    pub(crate) oid: gix::ObjectId,
}

/// `GET /object/<hash>` — return the serialized object: git's native
/// `<type> <size>\0<content>` form (uncompressed).
pub(crate) fn get_object(config: &Config, hash: &str) -> Result<Vec<u8>, HttpError> {
    let repo = config.repo.to_thread_local();
    let id = gix::ObjectId::from_hex(hash.as_bytes())
        .map_err(|err| HttpError::new(400, format!("invalid hash: {err}")))?;
    let object = repo
        .find_object(id)
        .map_err(|err| HttpError::new(404, format!("object not found: {err}")))?;
    let mut out = format!("{} {}\0", object.kind, object.data.len()).into_bytes();
    out.extend_from_slice(&object.data);
    Ok(out)
}

/// `POST /object/` — store a serialized object (`<type> <size>\0<content>`) and
/// return its hash (hex + `\n`). The type and size come from the body's header.
pub(crate) fn post_object(config: &Config, body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let repo = config.repo.to_thread_local();
    let (kind, content) = parse_posted_object(body)?;
    let id = match kind {
        gix::object::Kind::Blob => repo
            .write_blob(content)
            .map_err(|err| HttpError::new(500, format!("failed to write blob: {err}")))?
            .detach(),
        gix::object::Kind::Tree => {
            // Validate the canonical tree encoding before writing it as a real
            // tree object (so its hash is a genuine git tree hash).
            let tree = gix::objs::TreeRef::from_bytes(content, repo.object_hash())
                .map_err(|err| HttpError::new(400, format!("invalid tree: {err}")))?;
            repo.write_object(&tree)
                .map_err(|err| HttpError::new(500, format!("failed to write tree: {err}")))?
                .detach()
        }
        gix::object::Kind::Commit => {
            // Validate the commit encoding, then store the *raw* bytes (rather
            // than re-encoding the parsed form), so the hash is exactly what the
            // client computed over the bytes it sent.
            gix::objs::CommitRef::from_bytes(content, repo.object_hash())
                .map_err(|err| HttpError::new(400, format!("invalid commit: {err}")))?;
            gix::objs::Write::write_buf(&repo.objects, gix::object::Kind::Commit, content)
                .map_err(|err| HttpError::new(500, format!("failed to write commit: {err}")))?
        }
        other => {
            return Err(HttpError::new(
                400,
                format!("unsupported object type: {other} (expected blob, tree, or commit)"),
            ))
        }
    };
    Ok(format!("{id}\n").into_bytes())
}

/// Split a posted serialized object into its type and content, validating the
/// header (`<type> <size>\0`).
fn parse_posted_object(body: &[u8]) -> Result<(gix::object::Kind, &[u8]), HttpError> {
    let nul = body
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| HttpError::new(400, "malformed object: missing NUL after header"))?;
    let header = std::str::from_utf8(&body[..nul])
        .map_err(|_| HttpError::new(400, "malformed object header"))?;
    let content = &body[nul + 1..];
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| HttpError::new(400, "malformed object header: expected '<type> <size>'"))?;
    let size: usize = size
        .parse()
        .map_err(|_| HttpError::new(400, format!("malformed object size: {size:?}")))?;
    if size != content.len() {
        return Err(HttpError::new(
            400,
            format!("object size {size} != content length {}", content.len()),
        ));
    }
    let kind = gix::object::Kind::from_bytes(kind.as_bytes())
        .map_err(|_| HttpError::new(400, format!("unknown object type: {kind:?}")))?;
    Ok((kind, content))
}

/// Store a blob in the object database, returning its id. Compute uses this to
/// build the args/request objects for promise sub-runs (see `compute`); the
/// shape matches what a client would POST, so the hashes — and therefore the
/// cache keys — are identical no matter who builds the request.
pub(crate) fn store_git_blob(config: &Config, content: &[u8]) -> Result<gix::ObjectId, String> {
    let repo = config.repo.to_thread_local();
    repo.write_blob(content)
        .map(|id| id.detach())
        .map_err(|e| format!("writing blob: {e}"))
}

/// Encode `entries` as a git tree (sorted into git's required order) and store
/// it, returning its id. The server-side counterpart of the client's `post_tree`.
pub(crate) fn store_git_tree(
    config: &Config,
    mut entries: Vec<gix::objs::tree::Entry>,
) -> Result<gix::ObjectId, String> {
    entries.sort();
    let repo = config.repo.to_thread_local();
    repo.write_object(&gix::objs::Tree { entries })
        .map(|id| id.detach())
        .map_err(|e| format!("writing tree: {e}"))
}

/// Fetch and parse a git tree from the in-process object database.
pub(crate) fn fetch_tree(config: &Config, hash: &str) -> Result<Vec<TreeEntry>, String> {
    let (kind, content) = fetch_object(config, hash)?;
    if kind != "tree" {
        return Err(format!("expected tree, got {kind} for {hash}"));
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed tree {hash}: {e}"))?;
    Ok(tree
        .entries
        .iter()
        .map(|e| TreeEntry {
            name: String::from_utf8_lossy(e.filename).into_owned(),
            mode: e.mode,
            oid: e.oid.to_owned(),
        })
        .collect())
}

/// Fetch a git blob's bytes from the in-process object database.
pub(crate) fn fetch_blob(config: &Config, hash: &str) -> Result<Vec<u8>, String> {
    let (kind, content) = fetch_object(config, hash)?;
    if kind != "blob" {
        return Err(format!("expected blob, got {kind} for {hash}"));
    }
    Ok(content)
}

/// Read a git object from the in-process object database, returning its
/// `(type, content)`.
fn fetch_object(config: &Config, hash: &str) -> Result<(String, Vec<u8>), String> {
    let repo = config.repo.to_thread_local();
    let id = gix::ObjectId::from_hex(hash.as_bytes())
        .map_err(|e| format!("invalid hash {hash}: {e}"))?;
    let object = repo
        .find_object(id)
        .map_err(|e| format!("object {hash} not found: {e}"))?;
    Ok((object.kind.to_string(), object.data.clone()))
}
