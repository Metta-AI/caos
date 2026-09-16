//! Editable tool inputs assembled from the ordinary CAS primitives.
//! This runs as the worker, never as the privileged storage client.
use crate::{caos, cas_hash, path};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path};

/// Prepare a writable input for bash or an inline edit. Only selected paths
/// are fetched; untouched entries remain links to immutable CAS values.
/// Commit directories carry their original identity through cp -a and mv,
/// allowing put to restore their Git boundaries when the tool finishes.
pub fn materialize(source: &str, destination: &Path, paths: &[String]) -> Result<(), String> {
    for selected in paths {
        if selected != "."
            && (selected.is_empty()
                || Path::new(selected)
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_))))
        {
            return Err(format!("tool path must be relative: {selected:?}"));
        }
    }
    if let Ok(meta) = fs::symlink_metadata(destination) {
        if !meta.is_dir()
            || fs::read_dir(destination)
                .map_err(|e| e.to_string())?
                .next()
                .is_some()
        {
            return Err("tool destination must be absent or an empty directory".into());
        }
    }
    copy(Path::new(source), destination, "", paths, 0)
}

fn copy(
    source: &Path,
    destination: &Path,
    relative: &str,
    paths: &[String],
    depth: usize,
) -> Result<(), String> {
    if depth > 256 {
        return Err("tool input nesting exceeds 256 levels".into());
    }
    caos(["get", path(source)])?;
    if xattr::get(source, "user.caos.kind")
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some(b"commit")
    {
        let oid = cas_hash(path(source))?;
        // Keep the original commit in the protected CAS for put's validation.
        let original = format!("/cas/checkout-{oid}");
        fetch(&oid, &original)?;
        // Only the tree header is text; imported commit messages may use
        // another encoding.
        let raw = fs::read(&original).map_err(|e| e.to_string())?;
        let header = raw.split(|b| *b == b'\n').next().unwrap_or_default();
        let tree_oid = std::str::from_utf8(
            header
                .strip_prefix(b"tree ")
                .ok_or("commit has no tree header")?,
        )
        .map_err(|e| e.to_string())?;
        let tree = format!("/cas/checkout-{tree_oid}");
        fetch(tree_oid, &tree)?;
        if !Path::new(&tree).is_dir() {
            return Err("commit does not reference a tree".into());
        }
        copy(Path::new(&tree), destination, relative, paths, depth + 1)?;
        xattr::set(destination, "user.caos.commit", oid.as_bytes()).map_err(|e| e.to_string())?;
    } else if source.is_dir() {
        if !destination.exists() {
            fs::create_dir(destination).map_err(|e| e.to_string())?;
        }
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let relative = if relative.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{relative}/{}", name.to_string_lossy())
            };
            let source = entry.path();
            let destination = destination.join(name);
            if source.is_symlink() {
                symlink(
                    fs::read_link(source).map_err(|e| e.to_string())?,
                    destination,
                )
                .map_err(|e| e.to_string())?;
            } else if paths.iter().any(|p| {
                p == "."
                    || p == &relative
                    || p.starts_with(&format!("{relative}/"))
                    || relative.starts_with(&format!("{p}/"))
            }) {
                copy(&source, &destination, &relative, paths, depth + 1)?;
            } else {
                symlink(source, destination).map_err(|e| e.to_string())?;
            }
        }
    } else {
        fs::copy(source, destination).map_err(|e| e.to_string())?;
        let executable = fs::metadata(source)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111
            != 0;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// CAS paths are write-once. Multiple source entries can share a commit or
// tree, and successive inline edits also reuse this worker's CAS.
fn fetch(oid: &str, destination: &str) -> Result<(), String> {
    if Path::new(destination).exists() {
        if cas_hash(destination)? != oid {
            return Err("tool input cache identity mismatch".into());
        }
        caos(["get", destination])
    } else {
        caos(["get-hash", oid, destination])
    }
}
