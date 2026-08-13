//! Crash repair for the object database, run once at startup.
//!
//! Loose objects and loose refs are published the same way: write a temp file,
//! `rename()` it into place. Neither writer fsyncs in between by default — gix
//! never does (`gix-odb`'s loose store just calls `persist`), and `git` only
//! does under `core.fsync`, which `main` now sets. On ext4 the rename can reach
//! the journal before the data reaches the disk, so an unclean shutdown leaves
//! the right FILENAME holding ZERO BYTES. Observed on a host that went down
//! mid-turn: four objects and two refs, all stamped in the last 1.5 seconds
//! before the crash.
//!
//! One such object takes the whole server down for writes. `git-receive-pack`
//! validates every advertised ref before accepting an update, so a single
//! unreadable ref rejects every push, including pushes that never mention it.
//!
//! So we sweep at startup, in the same spirit as the repo config `main` reasserts
//! on every boot: expected damage, fixed each time we start. Content-addressed
//! request/result refs are disposable when broken. A conversation's sole
//! append-only `head` is not: deleting it would turn recoverable crash damage
//! into silent conversation loss. Its reflog is retained indefinitely, so
//! startup rolls the broken tip back to the newest intact logged commit. If no
//! intact entry exists, startup reports and preserves the ref for manual repair.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Delete every zero-length loose object under `git_dir`, returning how many
/// went.
///
/// Runs BEFORE anything opens the repo, so gix never caches one of these as
/// present: its loose-object lookup is a path-exists check, which an empty file
/// passes (see `storage::stored_despite`, which had the same blind spot).
pub(crate) fn sweep_empty_loose_objects(git_dir: &str) -> usize {
    let objects = Path::new(git_dir).join("objects");
    let Ok(fanout) = std::fs::read_dir(&objects) else {
        return 0;
    };
    let mut removed = 0;
    for dir in fanout.flatten() {
        // Only the 256 two-hex-digit fanout directories hold loose objects.
        // `pack` and `info` live here too and are not ours to sweep — a
        // zero-length file in there is a different problem with a different fix.
        let name = dir.file_name();
        let name = name.to_string_lossy();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_file() && meta.len() == 0 => {}
                _ => continue,
            }
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    eprintln!("repair: removed empty loose object {}", path.display());
                    removed += 1;
                }
                Err(err) => eprintln!("repair: cannot remove {}: {err}", path.display()),
            }
        }
    }
    removed
}

/// Delete every disposable loose ref that does not name a readable object,
/// returning how many went: an empty or unparsable ref file (what a crash
/// leaves behind), or one whose target object is missing from the store.
/// Canonical conversation heads are reported and preserved.
///
/// Must run AFTER [`sweep_empty_loose_objects`], so the existence question is
/// asked of a store the empty files are already out of.
///
/// Loose refs only. A ref that also has a packed entry falls back to it, which
/// is the outcome we want; and `packed-refs` is written under a lock and
/// replaced whole, so it does not fail this way.
pub(crate) fn drop_broken_refs(repo: &gix::Repository, git_dir: &str) -> usize {
    let mut paths = Vec::new();
    collect_files(&Path::new(git_dir).join("refs"), &mut paths);
    let mut removed = 0;
    for path in paths {
        let Some(reason) = breakage(repo, &path) else {
            continue;
        };
        let name = ref_name(git_dir, &path);
        if is_conversation_head(&name) && recover_conversation_head(repo, git_dir, &name, &path) {
            continue;
        }
        if is_conversation_head(&name) {
            eprintln!(
                "repair: preserving broken canonical conversation ref {name} ({reason}); manual recovery required"
            );
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                eprintln!("repair: removed broken ref {name} ({reason})");
                removed += 1;
            }
            Err(err) => eprintln!("repair: cannot remove {name}: {err}"),
        }
    }
    removed
}

fn is_conversation_head(name: &str) -> bool {
    name.strip_prefix("refs/caos/conversations/")
        .and_then(|rest| rest.strip_suffix("/head"))
        .is_some_and(|id| !id.is_empty())
}

/// Replace a broken canonical head with the newest intact commit named by its
/// reflog. The server enables reflogs for all refs and never expires them; this
/// is the recovery half of that policy.
fn recover_conversation_head(
    repo: &gix::Repository,
    git_dir: &str,
    name: &str,
    path: &Path,
) -> bool {
    let log = Path::new(git_dir).join("logs").join(name);
    let Ok(contents) = std::fs::read_to_string(&log) else {
        return false;
    };
    for line in contents.lines().rev() {
        let mut fields = line.split_whitespace();
        let old = fields.next().unwrap_or("");
        let new = fields.next().unwrap_or("");
        // Try the new value first, then the value it replaced. On a crash the
        // newest new object may be the missing one while its old value is the
        // last fully durable event.
        for candidate in [new, old] {
            let Ok(id) = gix::ObjectId::from_hex(candidate.as_bytes()) else {
                continue;
            };
            let intact_commit = repo
                .find_object(id)
                .is_ok_and(|object| object.kind == gix::object::Kind::Commit);
            if !intact_commit {
                continue;
            }
            match replace_ref_file(path, candidate) {
                Ok(()) => {
                    eprintln!(
                        "repair: restored canonical conversation ref {name} to {candidate} from its reflog"
                    );
                    return true;
                }
                Err(error) => {
                    eprintln!("repair: cannot restore {name} from its reflog: {error}");
                    return false;
                }
            }
        }
    }
    false
}

/// Publish a repaired loose ref with the same write/sync/rename/sync sequence
/// Git's `core.fsync=reference` gives normal updates. Startup is single-writer,
/// so a process-unique sibling is sufficient as the lock file here.
fn replace_ref_file(path: &Path, hash: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("ref has no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("ref has no UTF-8 file name"))?;
    let temporary = parent.join(format!(".{name}.repair-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| {
        writeln!(file, "{hash}")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })() {
        std::fs::remove_file(&temporary).ok();
        return Err(error);
    }
    Ok(())
}

/// Why `path` is not a usable ref, or `None` if it is fine (or if we could not
/// read it — an unreadable ref file is not evidence of a lost object, so it
/// keeps the benefit of the doubt).
fn breakage(repo: &gix::Repository, path: &Path) -> Option<String> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("repair: cannot read {}: {err}", path.display());
            return None;
        }
    };
    let text = String::from_utf8_lossy(&content);
    let text = text.trim();
    if text.is_empty() {
        return Some("empty file".into());
    }
    // A symbolic ref names another ref rather than an object, so there is no
    // object here to have lost.
    if text.starts_with("ref:") {
        return None;
    }
    let Ok(id) = gix::ObjectId::from_hex(text.as_bytes()) else {
        return Some(format!("unparsable content {text:?}"));
    };
    (!repo.has_object(id)).then(|| format!("object {id} is missing"))
}

/// Every regular file under `dir`, recursively.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&path, out),
            Ok(kind) if kind.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// A ref file's path rendered as its refname (`refs/caos/…`), for logging.
fn ref_name(git_dir: &str, path: &Path) -> String {
    path.strip_prefix(git_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_repo() -> (gix::ThreadSafeRepository, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("caos-repair-test-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let repo = gix::init_bare(&dir).unwrap().into_sync();
        (repo, dir)
    }

    /// Write a loose object file of `len` bytes at `id`'s path, whatever the
    /// content — the crash leaves a file whose name says one thing and whose
    /// bytes say another, and that is exactly what we are reproducing.
    fn plant_object(dir: &Path, id: &str, len: usize) -> PathBuf {
        let fanout = dir.join("objects").join(&id[..2]);
        std::fs::create_dir_all(&fanout).unwrap();
        let path = fanout.join(&id[2..]);
        std::fs::write(&path, vec![b'x'; len]).unwrap();
        path
    }

    fn plant_ref(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn empty_commit(dir: &Path, message: &str) -> String {
        let tree = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "hash-object",
                "-t",
                "tree",
                "--stdin",
            ])
            .output()
            .unwrap();
        assert!(tree.status.success());
        let tree = String::from_utf8(tree.stdout).unwrap();
        let commit = Command::new("git")
            .env("GIT_AUTHOR_NAME", "caos")
            .env("GIT_AUTHOR_EMAIL", "caos@caos")
            .env("GIT_COMMITTER_NAME", "caos")
            .env("GIT_COMMITTER_EMAIL", "caos@caos")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "commit-tree",
                tree.trim(),
                "-m",
                message,
            ])
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        String::from_utf8(commit.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn sweep_takes_empty_loose_objects_and_leaves_everything_else() {
        let (_repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();

        let empty = plant_object(&dir, "68173e37cae6a53970ceaf3a7d5ced68d1ce6d6a", 0);
        let intact = plant_object(&dir, "2880018d73cfc160c97955b13fd6fd96963c8647", 42);
        // A zero-length file that is not in a fanout directory is not ours.
        let pack_dir = dir.join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let pack_junk = pack_dir.join("pack-deadbeef.keep");
        std::fs::write(&pack_junk, b"").unwrap();

        assert_eq!(sweep_empty_loose_objects(&git_dir), 1);
        assert!(!empty.exists());
        assert!(intact.exists());
        assert!(pack_junk.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The one object you would expect to be a false positive is not one. The
    /// empty blob has empty CONTENT, but a loose object file is a zlib stream of
    /// `<type> <size>\0<content>` — the header `blob 0\0` alone is 7 bytes,
    /// plus zlib's 2-byte header and 4-byte adler32 trailer — so it lands on
    /// disk at 15 bytes. No valid loose object serializes to zero bytes, which
    /// is what makes "length 0" a safe test: the sweep measures the FILE, never
    /// the content.
    #[test]
    fn the_empty_blob_is_not_an_empty_file() {
        let (repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();
        let repo = repo.to_thread_local();

        let empty = repo.write_blob(b"").unwrap().detach();
        assert_eq!(
            empty.to_string(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        let hex = empty.to_string();
        let path = dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        assert!(std::fs::metadata(&path).unwrap().len() > 0);

        assert_eq!(sweep_empty_loose_objects(&git_dir), 0);
        assert!(path.exists());
        assert!(repo.find_object(empty).unwrap().data.is_empty());
        // And a ref naming it is sound, so the second half leaves it alone too.
        let good = plant_ref(&dir, "refs/caos/empty", &format!("{empty}\n"));
        assert_eq!(drop_broken_refs(&repo, &git_dir), 0);
        assert!(good.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_refs_go_and_sound_ones_stay() {
        let (repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();
        let repo = repo.to_thread_local();
        let real = repo.write_blob(b"a real object").unwrap().detach();

        let good = plant_ref(&dir, "refs/caos/good", &format!("{real}\n"));
        let symbolic = plant_ref(&dir, "refs/caos/symbolic", "ref: refs/caos/good\n");
        let empty = plant_ref(&dir, "refs/caos/req/crashed", "");
        let garbage = plant_ref(&dir, "refs/caos/req/garbled", "not a hash\n");
        let missing = plant_ref(
            &dir,
            "refs/caos/conversations/c95/status",
            "68173e37cae6a53970ceaf3a7d5ced68d1ce6d6a\n",
        );

        assert_eq!(drop_broken_refs(&repo, &git_dir), 3);
        assert!(good.exists());
        assert!(symbolic.exists());
        assert!(!empty.exists());
        assert!(!garbage.exists());
        assert!(!missing.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The end-to-end shape of the observed crash: a ref pointing at an object
    /// whose file survived the rename with no bytes in it. The sweep has to run
    /// first, or the ref check asks gix about a path that still exists and is
    /// told the object is fine.
    #[test]
    fn sweep_then_refs_clears_a_crashed_status_ref() {
        let (repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();
        let id = "68173e37cae6a53970ceaf3a7d5ced68d1ce6d6a";
        plant_object(&dir, id, 0);
        let status = plant_ref(&dir, "refs/caos/conversations/c95/status", id);

        assert_eq!(sweep_empty_loose_objects(&git_dir), 1);
        assert_eq!(drop_broken_refs(&repo.to_thread_local(), &git_dir), 1);
        assert!(!status.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_canonical_conversation_head_is_never_deleted() {
        let (repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();
        let head = plant_ref(
            &dir,
            "refs/caos/conversations/c95/head",
            "68173e37cae6a53970ceaf3a7d5ced68d1ce6d6a\n",
        );

        assert_eq!(drop_broken_refs(&repo.to_thread_local(), &git_dir), 0);
        assert!(head.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_canonical_head_rolls_back_to_latest_intact_reflog_commit() {
        let (repo, dir) = temp_repo();
        let git_dir = dir.to_string_lossy().into_owned();
        let intact = empty_commit(&dir, "intact event");
        let missing = "68173e37cae6a53970ceaf3a7d5ced68d1ce6d6a";
        let head = plant_ref(
            &dir,
            "refs/caos/conversations/c95/head",
            &format!("{missing}\n"),
        );
        plant_ref(
            &dir,
            "logs/refs/caos/conversations/c95/head",
            &format!(
                "0000000000000000000000000000000000000000 {intact} caos <caos@caos> 0 +0000\tcreated\n\
                 {intact} {missing} caos <caos@caos> 1 +0000\tcrashed append\n"
            ),
        );

        assert_eq!(drop_broken_refs(&repo.to_thread_local(), &git_dir), 0);
        assert_eq!(
            std::fs::read_to_string(&head).unwrap(),
            format!("{intact}\n")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_exact_conversation_head_shape_is_protected() {
        assert!(is_conversation_head(
            "refs/caos/conversations/project/chat/head"
        ));
        assert!(!is_conversation_head(
            "refs/caos/conversations/project/chat/status"
        ));
        assert!(!is_conversation_head("refs/caos/conversations/head"));
        assert!(!is_conversation_head("refs/caos/req/head"));
    }
}
