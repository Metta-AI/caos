//! The object store is git loose objects and nothing else
//! (design/loose-object-store.md).
//!
//! Every object is one file, `objects/xx/yyyy...`, holding a zlib stream of
//! `<type> <size>\0<content>` written at level 0 (deflate "stored" blocks):
//! the raw bytes plus a few bytes of framing. The filesystem compresses (prod
//! runs btrfs with zstd); level 0 keeps that compression effective and makes
//! inflating a copy. Every git and gix reader handles it, and since an object
//! id hashes the UNCOMPRESSED bytes, the level never changes an id: objects
//! written by git at other levels sit beside these.
//!
//! Nothing packs this store, so nothing ever rewrites an object under a
//! reader. What packs used to give for free is ATOMIC PUBLICATION: a pack
//! becomes visible when its index appears, all objects at once. Loose objects
//! appear one file at a time, so every path that brings in many objects at once
//! goes through [`Staging`]: the objects are written and verified somewhere
//! private on the same filesystem, synced once, then renamed into place
//! children first. A reader never sees an object whose references are missing,
//! and a crash part way leaves a closure-complete subset.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) type Kind = gix::object::Kind;
pub(crate) type Id = gix::ObjectId;

/// The argument the server binary is run with as the repository's
/// `pre-receive` hook (see [`publish_quarantine`]).
pub(crate) const HOOK_ARG: &str = "publish-quarantine";

/// Proves the `pre-receive` hook is ours, so a restart may rewrite it.
pub(crate) const HOOK_MARKER: &str =
    "# managed by caos-server: publish pushed objects in dependency order";

/// The hook's environment variable naming the server binary. `git.rs` sets it
/// on every `http-backend` it spawns; the hook passes it nothing else.
pub(crate) const SERVER_EXE_ENV: &str = "CAOS_SERVER_EXE";

/// The `pre-receive` hook. It is the same for every server sharing the
/// repository: each request's own server names itself in the environment, so
/// two builds on one repo never fight over the file's contents.
pub(crate) fn hook_script() -> String {
    format!(
        "#!/bin/sh\n{HOOK_MARKER}\n\
         if [ -z \"${SERVER_EXE_ENV}\" ]; then\n\
         \x20 echo \"caos: push through the caos server, which publishes pushed objects in dependency order\" >&2\n\
         \x20 exit 1\n\
         fi\n\
         exec \"${SERVER_EXE_ENV}\" {HOOK_ARG}\n"
    )
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// `objects/xx/yyyy...` for `id`, and its fan-out directory.
fn object_path(objects: &Path, id: &Id) -> (PathBuf, PathBuf) {
    let hex = id.to_string();
    let dir = objects.join(&hex[..2]);
    let path = dir.join(&hex[2..]);
    (dir, path)
}

/// A zlib stream of `data` at level 0: stored blocks of at most 65,535 bytes.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + (data.len() / 65_535 + 1) * 5 + 6);
    // CMF 0x78 (deflate, 32K window), FLG 0x01 (level 0; 0x7801 % 31 == 0).
    out.extend_from_slice(&[0x78, 0x01]);
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        let mut chunks = data.chunks(65_535).peekable();
        while let Some(chunk) = chunks.next() {
            out.push(u8::from(chunks.peek().is_none()));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5,552 bytes is the most that can be summed before `b` could overflow.
    for chunk in data.chunks(5_552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// The id of `(kind, data)` and its loose-object file contents.
fn encode(kind: Kind, data: &[u8]) -> io::Result<(Id, Vec<u8>)> {
    let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, kind, data)
        .map_err(|error| other(format!("hashing {kind}: {error}")))?;
    let mut raw = format!("{kind} {}\0", data.len()).into_bytes();
    raw.extend_from_slice(data);
    Ok((id, deflate_stored(&raw)))
}

/// A fresh name for a temporary file in `dir`, unique across threads and
/// processes sharing the store. Not a valid object name, so no reader takes it
/// for one, and [`sweep_temporaries`] knows it by its prefix.
fn temp_path(dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(
        "{TEMP_PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

const TEMP_PREFIX: &str = "tmp_caos_";

/// Write `bytes` to a new temporary file in `dir`, mode 0444 like git's, and
/// return its path. `sync` fsyncs the data before returning.
fn write_temp(dir: &Path, bytes: &[u8], sync: bool) -> io::Result<PathBuf> {
    let path = temp_path(dir);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)?;
        file.write_all(bytes)?;
        if sync {
            file.sync_all()?;
        }
        Ok(())
    })();
    match written {
        Ok(()) => Ok(path),
        Err(error) => {
            let _ = fs::remove_file(&path);
            Err(error)
        }
    }
}

/// Whether `path` holds a stored object. A zero-length file is a crash's
/// leftover, not an object (see [`crate::repair`]): it is replaced.
fn is_stored(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
}

/// Store one object durably and return its id: written to a temporary file,
/// fsynced, renamed into place, and the fan-out directory fsynced so the name
/// survives a crash too. The caller has already checked that everything the
/// object references is stored. Writing an object that is already there is a
/// no-op, and two writers racing on the same object both succeed: the rename
/// replaces identical content.
pub(crate) fn write_durable(objects: &Path, kind: Kind, data: &[u8]) -> io::Result<Id> {
    let (id, bytes) = encode(kind, data)?;
    let (dir, path) = object_path(objects, &id);
    if is_stored(&path) {
        return Ok(id);
    }
    fs::create_dir_all(&dir)?;
    let temp = write_temp(objects, &bytes, true)?;
    if let Err(error) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    File::open(&dir)?.sync_all()?;
    Ok(id)
}

/// Remove the temporary files a crash left in `objects/`. Called at startup,
/// before anything writes.
pub(crate) fn sweep_temporaries(objects: &Path) -> usize {
    let Ok(entries) = fs::read_dir(objects) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(TEMP_PREFIX)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Flush every pending write on the filesystem holding `path`: one flush for a
/// whole batch, instead of one per object.
fn sync_filesystem(path: &Path) -> io::Result<()> {
    let file = File::open(path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            // libc, resolved against what std already links.
            fn syncfs(fd: std::ffi::c_int) -> std::ffi::c_int;
        }
        if unsafe { syncfs(file.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        extern "C" {
            fn sync();
        }
        drop(file);
        unsafe { sync() };
        Ok(())
    }
}

/// The objects `data` references that must be stored before it: a tree's
/// entries (gitlinks excepted: they name separate histories), a commit's tree
/// and parents, a tag's target.
fn references(kind: Kind, data: &[u8]) -> io::Result<Vec<Id>> {
    match kind {
        Kind::Blob => Ok(Vec::new()),
        Kind::Tree => {
            let tree = gix::objs::TreeRef::from_bytes(data, gix::hash::Kind::Sha1)
                .map_err(|error| other(format!("malformed tree: {error}")))?;
            Ok(tree
                .entries
                .iter()
                .filter(|entry| !entry.mode.is_commit())
                .map(|entry| entry.oid.to_owned())
                .collect())
        }
        Kind::Commit => header_ids(data, &[b"tree ", b"parent "]),
        Kind::Tag => header_ids(data, &[b"object "]),
    }
}

/// The ids named by the header lines starting with one of `keys`, up to the
/// blank line that ends a commit's or tag's header. Continuation lines start
/// with a space, so an embedded tag (`mergetag`) or signature never matches.
fn header_ids(data: &[u8], keys: &[&[u8]]) -> io::Result<Vec<Id>> {
    let mut ids = Vec::new();
    for line in data.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            break;
        }
        for key in keys {
            if let Some(hex) = line.strip_prefix(*key) {
                ids.push(
                    Id::from_hex(hex)
                        .map_err(|error| other(format!("malformed object header: {error}")))?,
                );
            }
        }
    }
    Ok(ids)
}

/// The order to publish `children`'s keys in: every object after each of its
/// references that is also staged. A depth-first walk renaming in post-order;
/// content addressing rules out cycles.
fn publication_order(children: &HashMap<Id, Vec<Id>>) -> Vec<Id> {
    let mut order = Vec::with_capacity(children.len());
    let mut done = HashSet::with_capacity(children.len());
    let mut expanded = HashSet::with_capacity(children.len());
    for root in children.keys() {
        let mut stack = vec![(*root, false)];
        while let Some((id, ready)) = stack.pop() {
            if done.contains(&id) {
                continue;
            }
            if ready {
                done.insert(id);
                order.push(id);
                continue;
            }
            if !expanded.insert(id) {
                continue;
            }
            stack.push((id, true));
            for child in &children[&id] {
                if children.contains_key(child) && !done.contains(child) {
                    stack.push((*child, false));
                }
            }
        }
    }
    order
}

/// Loose objects waiting, in a private directory on the store's filesystem, to
/// be published children first.
pub(crate) struct Staging {
    dir: PathBuf,
    children: HashMap<Id, Vec<Id>>,
}

impl Staging {
    /// An empty staging area at `dir`, which its owner removes afterwards.
    pub(crate) fn new(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            children: HashMap::new(),
        })
    }

    /// Stage one object. Not synced: [`Staging::publish`] flushes them all at
    /// once.
    pub(crate) fn add(&mut self, kind: Kind, data: &[u8]) -> io::Result<Id> {
        let (id, bytes) = encode(kind, data)?;
        if self.children.contains_key(&id) {
            return Ok(id);
        }
        let (dir, path) = object_path(&self.dir, &id);
        fs::create_dir_all(&dir)?;
        let temp = write_temp(&self.dir, &bytes, false)?;
        fs::rename(&temp, &path)?;
        self.children.insert(id, references(kind, data)?);
        Ok(id)
    }

    /// How many objects are staged.
    pub(crate) fn len(&self) -> usize {
        self.children.len()
    }

    /// Move every staged object into `objects` (the live store, on the same
    /// filesystem), children first, and return how many. One flush makes the
    /// staged bytes durable before any name appears in the store, and one more
    /// makes the names durable before returning. The caller has verified that
    /// every reference that is not staged is already stored.
    pub(crate) fn publish(self, objects: &Path) -> io::Result<usize> {
        if self.children.is_empty() {
            return Ok(0);
        }
        sync_filesystem(&self.dir)?;
        let order = publication_order(&self.children);
        let mut made = HashSet::new();
        for id in &order {
            let (_, staged) = object_path(&self.dir, id);
            let (dir, path) = object_path(objects, id);
            if made.insert(dir.clone()) {
                fs::create_dir_all(&dir)?;
            }
            // Replacing an object already stored replaces identical content,
            // and heals a zero-length leftover of a crash.
            fs::rename(&staged, &path)
                .map_err(|error| other(format!("publishing {id}: {error}")))?;
        }
        sync_filesystem(objects)?;
        Ok(order.len())
    }
}

/// Every object `git cat-file --batch` prints, run as `command` (which names
/// the repository and the `cat-file --batch` arguments): `ids` on its stdin, or
/// nothing when the command enumerates objects itself (`--batch-all-objects`).
/// `each` is called once per object, in output order.
pub(crate) fn read_objects(
    mut command: Command,
    ids: Option<&[Id]>,
    mut each: impl FnMut(Id, Kind, Vec<u8>) -> io::Result<()>,
) -> io::Result<()> {
    command
        .stdin(if ids.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let writer = match (ids, child.stdin.take()) {
        (Some(ids), Some(mut stdin)) => {
            let list: String = ids.iter().map(|id| format!("{id}\n")).collect();
            Some(std::thread::spawn(move || {
                let _ = stdin.write_all(list.as_bytes());
            }))
        }
        _ => None,
    };
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut result = Ok(());
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                result = Err(error);
                break;
            }
        }
        let header = line.trim_end();
        let parsed = (|| {
            let mut fields = header.split(' ');
            let id = Id::from_hex(fields.next()?.as_bytes()).ok()?;
            let kind = Kind::from_bytes(fields.next()?.as_bytes()).ok()?;
            let size: usize = fields.next()?.parse().ok()?;
            Some((id, kind, size))
        })();
        let Some((id, kind, size)) = parsed else {
            result = Err(other(format!("git cat-file printed {header:?}")));
            break;
        };
        let mut data = vec![0; size];
        let mut newline = [0u8; 1];
        if let Err(error) = stdout
            .read_exact(&mut data)
            .and_then(|()| stdout.read_exact(&mut newline))
        {
            result = Err(error);
            break;
        }
        if let Err(error) = each(id, kind, data) {
            result = Err(error);
            break;
        }
    }
    if result.is_err() {
        let _ = child.kill();
    }
    drop(stdout);
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let output = child.wait_with_output()?;
    if result.is_ok() && !output.status.success() {
        result = Err(other(format!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    result
}

/// The ids of the loose objects under `objects`.
fn list_loose(objects: &Path) -> io::Result<Vec<Id>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(objects)? {
        let entry = entry?;
        let prefix = entry.file_name().to_string_lossy().into_owned();
        if prefix.len() != 2
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        for file in fs::read_dir(entry.path())? {
            let rest = file?.file_name().to_string_lossy().into_owned();
            if rest.len() == 38 && rest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                if let Ok(id) = Id::from_hex(format!("{prefix}{rest}").as_bytes()) {
                    ids.push(id);
                }
            }
        }
    }
    Ok(ids)
}

/// The `pre-receive` hook: publish a push's objects from git's quarantine into
/// the store, children first, before git updates any ref.
///
/// receive-pack has already unpacked the push into the quarantine as loose
/// objects (`receive.unpackLimit` is effectively infinite) and checked them
/// (`receive.fsckObjects`, and connectivity against the store). Left to
/// itself, git would then move them into the store in directory order, which
/// can expose a tree before its subtrees, and a crash part way would leave the
/// store with a missing child. Moved here first, git finds nothing left to
/// migrate. A push whose ref update then fails leaves closure-complete objects
/// behind, which is harmless.
///
/// Returns the process exit code: non-zero declines the push.
pub(crate) fn publish_quarantine() -> i32 {
    // git writes the ref updates to our stdin; read them so it never sees a
    // closed pipe.
    let _ = io::copy(&mut io::stdin(), &mut io::sink());
    // No quarantine: the push carried no objects (a ref-only update).
    let Some(quarantine) = std::env::var_os("GIT_QUARANTINE_PATH") else {
        return 0;
    };
    let git_dir = std::env::var_os("GIT_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from);
    match publish_quarantined(Path::new(&quarantine), &git_dir.join("objects")) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("caos: cannot publish the pushed objects: {error}");
            1
        }
    }
}

fn publish_quarantined(quarantine: &Path, objects: &Path) -> io::Result<usize> {
    let packs = quarantine.join("pack");
    if let Ok(entries) = fs::read_dir(&packs) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|ext| ext == "pack") {
                return Err(other(
                    "the push arrived as a pack; the server must run with receive.unpackLimit \
                     unset to infinity so pushes land as loose objects",
                ));
            }
        }
    }
    let ids = list_loose(quarantine)?;
    let mut staging = Staging {
        dir: quarantine.to_path_buf(),
        children: HashMap::with_capacity(ids.len()),
    };
    // The hook's environment points git at the quarantine, with the store as
    // its alternate, so this reads exactly the pushed objects.
    let mut command = Command::new("git");
    command.args(["cat-file", "--batch"]);
    read_objects(command, Some(&ids), |id, kind, data| {
        staging.children.insert(id, references(kind, &data)?);
        Ok(())
    })?;
    staging.publish(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("caos-loose-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn adler32_matches_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn written_objects_read_back_through_git() {
        let dir = scratch("write");
        let objects = dir.join("objects");
        // Empty, small, and larger than one stored block (65,535 bytes).
        let large: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        for content in [&b""[..], b"hello\n", &large] {
            let id = write_durable(&objects, Kind::Blob, content).unwrap();
            let shown = git(&dir, &["cat-file", "blob", &id.to_string()]);
            assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
            assert_eq!(shown.stdout, content);
            let (_, path) = object_path(&objects, &id);
            let size = fs::metadata(&path).unwrap().len() as usize;
            assert!(size >= content.len(), "level 0 never shrinks the content");
        }
        // Writing again is a no-op.
        write_durable(&objects, Kind::Blob, b"hello\n").unwrap();
        let fsck = git(&dir, &["fsck", "--strict"]);
        assert!(fsck.status.success(), "{}", String::from_utf8_lossy(&fsck.stderr));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids_match_git() {
        let dir = scratch("ids");
        let id = write_durable(&dir.join("objects"), Kind::Blob, b"hello\n").unwrap();
        assert_eq!(id.to_string(), "ce013625030ba8dba906f756967f9e9ca394464a");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn order_puts_references_first() {
        let id = |n: u8| Id::from_hex(format!("{n:02x}").repeat(20).as_bytes()).unwrap();
        // commit 1 -> tree 2 -> tree 3 -> blob 4; tree 2 -> blob 4; commit 1 -> parent 5 (not staged)
        let mut children = HashMap::new();
        children.insert(id(1), vec![id(2), id(5)]);
        children.insert(id(2), vec![id(3), id(4)]);
        children.insert(id(3), vec![id(4)]);
        children.insert(id(4), vec![]);
        let order = publication_order(&children);
        assert_eq!(order.len(), 4);
        let at = |n: u8| order.iter().position(|x| *x == id(n)).unwrap();
        assert!(at(4) < at(3) && at(3) < at(2) && at(2) < at(1));
    }

    #[test]
    fn staging_publishes_a_complete_closure() {
        let dir = scratch("staging");
        let objects = dir.join("objects");
        let mut staging = Staging::new(dir.join("staging")).unwrap();
        let blob = staging.add(Kind::Blob, b"content\n").unwrap();
        let mut tree = Vec::new();
        tree.extend_from_slice(b"100644 file\0");
        tree.extend_from_slice(blob.as_bytes());
        let tree = staging.add(Kind::Tree, &tree).unwrap();
        let commit = format!(
            "tree {tree}\nauthor a <a@a> 0 +0000\ncommitter a <a@a> 0 +0000\n\nm\n"
        );
        let commit = staging.add(Kind::Commit, commit.as_bytes()).unwrap();
        assert_eq!(staging.len(), 3);
        assert_eq!(staging.publish(&objects).unwrap(), 3);
        let walk = git(&dir, &["rev-list", "--objects", "--missing=error", &commit.to_string()]);
        assert!(walk.status.success(), "{}", String::from_utf8_lossy(&walk.stderr));
        assert_eq!(String::from_utf8_lossy(&walk.stdout).lines().count(), 3);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn references_follow_commits_and_tags_not_gitlinks() {
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let commit = format!(
            "tree {a}\nparent {b}\nauthor a <a@a> 0 +0000\nmergetag object {a}\n type commit\n\nparent {a}\n"
        );
        let ids = references(Kind::Commit, commit.as_bytes()).unwrap();
        assert_eq!(ids.len(), 2);
        let tag = format!("object {b}\ntype commit\ntag t\n\nmsg\n");
        assert_eq!(references(Kind::Tag, tag.as_bytes()).unwrap().len(), 1);
        let mut tree = Vec::new();
        tree.extend_from_slice(b"160000 sub\0");
        tree.extend_from_slice(&[0x11; 20]);
        assert!(references(Kind::Tree, &tree).unwrap().is_empty());
    }
}
