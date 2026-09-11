//! Writable projections of Git trees. Directory xattrs retain commit boundaries;
//! ordinary names and contents remain the only source-tree organization.
use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

const BASE: &str = "user.caos.commit";

pub struct Checkout(Node);

enum Node {
    Directory(Vec<(std::ffi::OsString, Node)>, Option<String>),
    File(PathBuf, u32),
    Link(PathBuf),
    Unloaded(PathBuf),
}

/// Prepare content while the CLI can still access the protected CAS. Writing
/// the projection happens separately, after the CLI drops its elevated uid.
pub fn prepare(t: &dyn Transport, hash: &str, paths: &[String]) -> Result<Checkout, String> {
    for path in paths {
        if path != "."
            && (path.is_empty()
                || Path::new(path)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_))))
        {
            return Err(format!("checkout path must be relative: {path:?}"));
        }
    }
    probe_xattr(&cas_dir())?;
    Ok(Checkout(load(t, hash, "", paths, 0)?))
}

fn location(hash: &str) -> Result<PathBuf, String> {
    let oid = gix::ObjectId::from_hex(hash.as_bytes()).map_err(|e| e.to_string())?;
    Ok(cas_dir().join(format!("checkout-{oid}")))
}

fn load(
    t: &dyn Transport,
    hash: &str,
    relative: &str,
    paths: &[String],
    depth: usize,
) -> Result<Node, String> {
    if depth > 256 {
        return Err("checkout nesting exceeds 256 levels".into());
    }
    let path = location(hash)?;
    if path.exists() {
        if read_hash(&path)? != hash {
            return Err("checkout cache identity mismatch".into());
        }
        expand(t, &path, Some(1))?;
    } else {
        fetch_and_materialize(t, &path, hash)?;
    }
    if result_kind(&path)? == "commit" {
        let raw = std::fs::read(&path).map_err(|e| e.to_string())?;
        let commit = gix::objs::CommitRef::from_bytes(&raw, gix::hash::Kind::Sha1)
            .map_err(|e| e.to_string())?;
        let tree = String::from_utf8_lossy(commit.tree).to_string();
        let Node::Directory(children, _) = load(t, &tree, relative, paths, depth + 1)? else {
            return Err("commit does not reference a tree".into());
        };
        return Ok(Node::Directory(children, Some(hash.to_string())));
    }
    if path.is_dir() {
        let mut children = Vec::new();
        for child in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
            let child = child.map_err(|e| e.to_string())?;
            let name = child.file_name();
            let rel = if relative.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{relative}/{}", name.to_string_lossy())
            };
            let selected = paths.iter().any(|p| {
                p == "."
                    || p == &rel
                    || p.starts_with(&format!("{rel}/"))
                    || rel.starts_with(&format!("{p}/"))
            });
            let child_path = child.path();
            let node = if child_path.is_symlink() {
                Node::Link(std::fs::read_link(&child_path).map_err(|e| e.to_string())?)
            } else if selected {
                let hash = read_hash(&child_path)?;
                let mut node = load(t, &hash, &rel, paths, depth + 1)?;
                if let Node::File(_, mode) = &mut node {
                    if cas_entry(&child_path)?.0.is_executable() {
                        *mode = 0o755;
                    }
                }
                node
            } else {
                Node::Unloaded(child_path)
            };
            children.push((name, node));
        }
        Ok(Node::Directory(children, None))
    } else {
        Ok(Node::File(path, 0o644))
    }
}

impl Checkout {
    /// Destination must be absent or an empty directory; never follows an
    /// existing destination symlink. Callers must write as the invoking user.
    pub fn write(self, destination: &Path) -> Result<(), String> {
        if let Ok(meta) = std::fs::symlink_metadata(destination) {
            if !meta.is_dir()
                || std::fs::read_dir(destination)
                    .map_err(|e| e.to_string())?
                    .next()
                    .is_some()
            {
                return Err("checkout destination must be absent or an empty directory".into());
            }
        }
        self.0.write(destination)
    }
}

impl Node {
    fn write(self, path: &Path) -> Result<(), String> {
        match self {
            Node::Directory(children, base) => {
                if !path.exists() {
                    std::fs::create_dir(path).map_err(|e| e.to_string())?;
                }
                for (name, node) in children {
                    node.write(&path.join(name))?;
                }
                if let Some(base) = base {
                    xattr::set(path, BASE, base.as_bytes()).map_err(|e| e.to_string())?;
                }
            }
            Node::File(source, mode) => {
                std::fs::copy(source, path).map_err(|e| e.to_string())?;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| e.to_string())?;
            }
            Node::Link(target) | Node::Unloaded(target) => {
                symlink(target, path).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// Turn a projected directory back into a commit, retaining the exact old
/// object (including signatures) when its tree is unchanged. The xattr only
/// names an object in the protected CAS; it cannot supply forged headers.
pub(super) fn commit(cas: Option<&Path>, path: &Path, tree: Hashed) -> Result<Hashed, String> {
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
