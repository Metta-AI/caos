//! Commit-valued entries: preserving edited boundaries and resolving paths.
use super::*;

const BASE: &str = "user.caos.commit";

/// Turn a projected directory back into a commit, retaining the exact old
/// object (including signatures) when its tree is unchanged. The xattr only
/// names an object in the protected CAS; it cannot supply forged headers.
pub(super) fn commit(
    t: &dyn Transport,
    cas: Option<&Path>,
    path: &Path,
    mut tree: Hashed,
) -> Result<Hashed, String> {
    let Some(base) = xattr::get(path, BASE).map_err(|e| e.to_string())? else {
        return Ok(tree);
    };
    let base = String::from_utf8(base).map_err(|e| e.to_string())?;
    let oid = gix::ObjectId::from_hex(base.as_bytes()).map_err(|e| e.to_string())?;
    let source = cas
        .ok_or("projected commits require a CAS")?
        .join(format!("checkout-{oid}"));
    if read_hash(&source)? != oid.to_string() || result_kind(&source)? != "commit" {
        return Err("projected commit provenance is not a CAS commit".into());
    }
    let raw = std::fs::read(source).map_err(|e| e.to_string())?;
    let original =
        gix::objs::CommitRef::from_bytes(&raw, gix::hash::Kind::Sha1).map_err(|e| e.to_string())?;
    if original.tree == tree.oid.to_string().as_bytes() {
        return Ok(Hashed {
            mode: gix::objs::tree::EntryKind::Commit.into(),
            oid,
            body: Body::Stored,
        });
    }
    clean_merge_metadata(t, &mut tree, true)?;
    let headers = raw
        .split(|b| *b == b'\n')
        .take_while(|line| !line.is_empty());
    let mut encoded = format!("tree {}\nparent {oid}\n", tree.oid).into_bytes();
    for line in headers {
        if line.starts_with(b"author ") || line.starts_with(b"committer ") {
            encoded.extend_from_slice(line);
            encoded.push(b'\n');
        }
    }
    encoded.extend_from_slice(b"\nEdit source tree\n");
    Ok(Hashed {
        mode: gix::objs::tree::EntryKind::Commit.into(),
        oid: hash_bytes("commit", &encoded)?,
        body: Body::Commit(encoded, Box::new(tree)),
    })
}

// Only edited commit projections use this rule. Plain trees (including the
// conversation protocol) and untouched commits are exact.
fn clean_merge_metadata(
    t: &dyn Transport,
    tree: &mut Hashed,
    source_root: bool,
) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};
    if matches!(tree.body, Body::Stored) {
        // Partial projections leave .caos lazy. Read only its tree; all of
        // its children are already stored, so none need to be sent again.
        let (kind, bytes) = t.get_object(&tree.oid.to_string())?;
        if kind != "tree" {
            return Err("source merge metadata is not a tree".into());
        }
        tree.body = Body::Dir(bytes, Vec::new());
    }
    let Body::Dir(bytes, children) = &mut tree.body else {
        return Ok(());
    };
    let mut entries: Vec<Entry> = gix::objs::TreeRef::from_bytes(bytes, gix::hash::Kind::Sha1)
        .map_err(|e| e.to_string())?
        .entries
        .into_iter()
        .map(|entry| Entry {
            mode: entry.mode,
            filename: entry.filename.to_owned(),
            oid: entry.oid.to_owned(),
        })
        .collect();
    if source_root {
        for entry in &mut entries {
            if entry.filename == b".caos" && entry.mode.kind() == EntryKind::Tree {
                if let Some(child) = children.iter_mut().find(|child| child.oid == entry.oid) {
                    clean_merge_metadata(t, child, false)?;
                    entry.oid = child.oid;
                }
            }
        }
    }
    let empty = hash_bytes(if source_root { "tree" } else { "blob" }, b"")?;
    entries.retain(|entry| {
        let removable = if source_root {
            entry.filename == b".caos" && entry.mode.kind() == EntryKind::Tree
        } else {
            entry.filename == b"conflicts"
                && matches!(
                    entry.mode.kind(),
                    EntryKind::Blob | EntryKind::BlobExecutable
                )
        };
        !(removable && entry.oid == empty)
    });
    children.retain(|child| entries.iter().any(|entry| entry.oid == child.oid));
    bytes.clear();
    gix::objs::Tree { entries }
        .write_to(bytes)
        .map_err(|e| e.to_string())?;
    tree.oid = hash_bytes("tree", bytes)?;
    Ok(())
}

/// Resolve a path through commit entries without projecting or copying content.
/// Names remain relative to the given immutable object; blobs containing SHAs
/// are ordinary blobs and are never interpreted as pointers.
pub fn resolve(
    t: &dyn Transport,
    hash: &str,
    relative: &str,
    destination: &str,
) -> Result<(), String> {
    let components: Vec<_> = Path::new(relative)
        .components()
        .filter(|part| *part != std::path::Component::CurDir)
        .collect();
    if components
        .iter()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("resolve requires a relative path without ..".into());
    }
    let mut oid = hash.to_string();
    let mut executable = false;
    for index in 0..=components.len() {
        let mut object = t.get_object(&oid)?;
        for depth in 0..=256 {
            if object.0 != "commit" {
                break;
            }
            if depth == 256 {
                return Err("commit nesting exceeds 256 levels".into());
            }
            let commit = gix::objs::CommitRef::from_bytes(&object.1, gix::hash::Kind::Sha1)
                .map_err(|e| e.to_string())?;
            oid = String::from_utf8_lossy(commit.tree).into_owned();
            drop(commit);
            object = t.get_object(&oid)?;
        }
        if index == components.len() {
            if executable {
                let target = validate_target(&cas_dir(), destination)?;
                probe_xattr(&cas_dir())?;
                return write_file_with_mode(&target, &oid, &object.0, &object.1, true);
            }
            return get_hash(t, &oid, destination);
        }
        if object.0 != "tree" {
            return Err(format!("{relative:?} traverses a non-directory"));
        }
        let tree = gix::objs::TreeRef::from_bytes(&object.1, gix::hash::Kind::Sha1)
            .map_err(|e| e.to_string())?;
        let name = components[index].as_os_str().as_bytes();
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.filename.as_ref() as &[u8] == name)
            .ok_or_else(|| format!("no such path: {relative}"))?;
        executable = entry.mode.is_executable();
        oid = entry.oid.to_string();
    }
    unreachable!()
}
