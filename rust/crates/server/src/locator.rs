//! `:@@=` locator resolution, SERVER-SIDE (design/flake-inputs.md).
//!
//! A locator names a tree in another repo by `url + rev`, and something has to
//! turn that pair into an oid. That used to be the client, and the reason given
//! was never a sandbox: a locator has to become an oid BEFORE the ArgTree is
//! formed, or the URL sits inside the cache key and two consumers pinning the
//! same rev through different URLs key identical content differently. That
//! reason is satisfied by whoever resolves it, as long as they resolve it
//! before forming the request — and the server forms requests, in
//! `caos_eval`'s walk, exactly where the client's did.
//!
//! What made the client the wrong place is the agent. A conversation's tree is
//! evaluated SERVER-SIDE (`eval_path`, the `eval` continuation), and a session
//! that starts from a caos-session commit has no source tree on anyone's disk to
//! fall back to. With resolution here, `eval_path ./caos-std` in a conversation
//! evaluates the repo's root expression, resolves its pin, and hands the model
//! the mounted tree; with it on the client, that walk died on a locator it was
//! structurally unable to follow.
//!
//! ## Three ways a rev becomes a tree, in order
//!
//! 1. **`refs/caos/locator-trees/<rev>`** — this server resolved that pin
//!    before. A rev is a commit sha, so `rev → tree` is a function of content:
//!    the answer cannot go stale and the ref is the whole memo, surviving a
//!    restart and shared between server processes on one object database.
//! 2. **`<rev>^{tree}`** — the commit is already here: pushed by a client,
//!    brought in by `POST /git/import`, or `refs/caos/dev` in a dev stack. This
//!    is the whole of how a `git+caos://…` locator resolves, and it must be:
//!    that URL names THIS server, and `git-remote-caos` is a client program.
//! 3. **A fetch**, `--depth=1`, into a disposable bare repo.
//!
//! **Only the TREE closure is published.** A `--depth=1` fetch has a shallow
//! boundary and an incomplete commit, and the server's startup check refuses a
//! store containing either (`verify_object_closure`). So the staging repo
//! keeps the commit and the `shallow` file, and what is packed into the live
//! object database is the tree and everything under it — complete by
//! construction, and byte-identical to what a `:@=` of the same content would
//! have produced. Nothing anchors the commit, which is correct: a locator wants
//! a snapshot, not a history.
//!
//! **`path:` is refused.** It names a live directory on the machine that wrote
//! the expression, and the server is not that machine. The error says so rather
//! than reporting "not found" for a path that exists perfectly well where the
//! author is looking.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git_locator::GitRef;

use crate::Config;

/// Where a resolved pin is recorded. Hidden from ref advertisement
/// (`configure_ref_advertisements`): it is a memo, not something a client
/// fetches by name.
pub(crate) const TREE_REF_PREFIX: &str = "refs/caos/locator-trees/";

/// Resolve a locator's ROOT — the tree of the commit it pins — leaving `dir=`
/// to the caller, which descends it through EVALUATION rather than a raw tree
/// walk (see `eval_remote_arg`'s note: a raw walk hands the evaluator a bare
/// `std/<x>` whose expression names `DEEP-DEPS/…` mounts the root expression
/// has not produced yet).
pub(crate) fn resolve_root(
    config: &Config,
    git_ref: &GitRef,
    token: Option<&str>,
) -> Result<gix::ObjectId, String> {
    if git_ref.is_plain_dir() {
        return Err(format!(
            "`{}` names a directory on the machine that wrote the expression, and \
             locators are resolved by the caos server, which cannot read one. Pin \
             the tree with `git+…?rev=<sha>` instead",
            git_ref.url
        ));
    }
    let rev = git_ref
        .rev
        .as_deref()
        .ok_or("a remote locator must pin a commit with `rev=<40-hex sha>`")?
        .to_ascii_lowercase();
    if !git_locator::import::commit(&rev) {
        return Err(format!("rev must be a full-length commit sha, got {rev:?}"));
    }

    if let Some(tree) = stored_tree(config, &rev) {
        return Ok(tree);
    }

    // SINGLE-FLIGHT, and re-read inside the lock. A fan-out of eval threads
    // reaches the same pin in the same instant (AGENTS.md: a cache read followed
    // by a cache write is check-then-act), and each miss here is a whole `git
    // fetch` of another repository.
    let directory = Path::new(&config.git_dir).join("caos-locators");
    fs::create_dir_all(&directory).map_err(|e| format!("creating {directory:?}: {e}"))?;
    let lock_path = directory.join(format!("{rev}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("opening {lock_path:?}: {e}"))?;
    lock.lock()
        .map_err(|e| format!("locking {lock_path:?}: {e}"))?;
    if let Some(tree) = stored_tree(config, &rev) {
        return Ok(tree);
    }

    let url = git_ref.fetch_url();
    if url.starts_with("caos://") {
        // THIS server, named by ticket. There is nothing to fetch from: the
        // `caos://` transport is a client git remote helper, and a locator
        // written this way (dev mode rewrites one, `bootstrap.go`) expects the
        // commit to be here already.
        return Err(format!(
            "commit {rev} is not in this server's object store, and \
             {url} names this server, so there is nowhere to fetch it from"
        ));
    }
    let tree = fetch_tree(config, &url, &rev, token)?;
    crate::run_required_git(&[
        "--git-dir",
        &config.git_dir,
        "update-ref",
        &format!("{TREE_REF_PREFIX}{rev}"),
        &tree.to_string(),
    ])?;
    Ok(tree)
}

/// Steps 1 and 2: the pin's memo ref, then the commit itself. Both are
/// `rev-parse`, so a ref pointing straight at a tree and a stored commit read
/// the same way.
fn stored_tree(config: &Config, rev: &str) -> Option<gix::ObjectId> {
    for name in [format!("{TREE_REF_PREFIX}{rev}"), rev.to_string()] {
        let output = git(&config.git_dir)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{tree}}"),
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Ok(oid) = gix::ObjectId::from_hex(text.trim().as_bytes()) {
                return Some(oid);
            }
        }
    }
    None
}

/// Step 3: fetch `rev` from `url` into a disposable bare repo, then publish only
/// its TREE closure into the live object database.
fn fetch_tree(
    config: &Config,
    url: &str,
    rev: &str,
    token: Option<&str>,
) -> Result<gix::ObjectId, String> {
    let staging = Staging::new(Path::new(&config.git_dir), rev)?;
    let remote = |args: &[&str]| -> Result<String, String> {
        let output = git_locator::locator::fetch_command(url, token)?
            .args(["--git-dir"])
            .arg(&staging.path)
            .args(args)
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !output.status.success() {
            // A remote's stderr can echo a credential, so `fetch_command`
            // discards it; the message says what was asked for instead.
            return Err(format!(
                "`git {}` failed for {url} at {rev}: check the repository, the \
                 commit, and this server's access to it",
                args.first().copied().unwrap_or("")
            ));
        }
        String::from_utf8(output.stdout).map_err(|e| format!("git output is not UTF-8: {e}"))
    };
    remote(&["init", "--bare", "--quiet"])?;
    remote(&[
        "fetch",
        "--quiet",
        "--depth=1",
        "--no-tags",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
        "--",
        url,
        rev,
    ])?;
    let tree = remote(&[
        "rev-parse",
        "--verify",
        &format!("{rev}^{{commit}}^{{tree}}"),
    ])?;
    let tree = tree.trim().to_string();
    let oid = gix::ObjectId::from_hex(tree.as_bytes())
        .map_err(|_| format!("git named {tree:?}, which is not an object id"))?;

    // `--revs` over a TREE packs that tree and everything under it, and nothing
    // above: no commit, no shallow boundary, so what lands in the live store is
    // complete on its own.
    let roots = staging.path.join("roots");
    fs::write(&roots, format!("{tree}\n")).map_err(|e| format!("writing {roots:?}: {e}"))?;
    let pack = staging.path.join("tree.pack");
    let packed = git(&staging.path)
        .args(["pack-objects", "--stdout", "--threads=1", "--revs"])
        .stdin(File::open(&roots).map_err(|e| format!("reading {roots:?}: {e}"))?)
        .stdout(File::create(&pack).map_err(|e| format!("creating {pack:?}: {e}"))?)
        .output()
        .map_err(|e| format!("packing the locator tree: {e}"))?;
    if !packed.status.success() {
        return Err(format!("could not pack {rev}'s tree from {url}"));
    }
    // `--strict` in the LIVE store: every link the pack makes must resolve,
    // the same guarantee `POST /object/` enforces for a pushed tree. A
    // locator's tree closure stands alone, so it passes on its own.
    let indexed = git(&config.git_dir)
        .args(["index-pack", "--strict", "--threads=1", "--stdin"])
        .stdin(File::open(&pack).map_err(|e| format!("reading {pack:?}: {e}"))?)
        .stdout(Stdio::null())
        .output()
        .map_err(|e| format!("storing the locator tree: {e}"))?;
    if !indexed.status.success() {
        return Err(format!(
            "the tree fetched for {rev} from {url} is incomplete: {}",
            String::from_utf8_lossy(&indexed.stderr).trim()
        ));
    }
    Ok(oid)
}

/// A hardened `git` over a local object database: no ambient config, no
/// replace/graft indirection, no lazy fetch, and no network — every caller here
/// is reading or writing a store this process owns.
fn git(git_dir: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new("git");
    command.env_clear();
    for name in ["PATH", "SSL_CERT_FILE", "GIT_SSL_CAINFO"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args(["-c", "core.commitGraph=false", "--git-dir"])
        .arg(git_dir);
    command
}

/// The disposable repository a locator fetch happens in. A failed fetch leaves
/// its shallow boundary and incomplete commit HERE, where nothing reads them,
/// and the directory goes away with this value.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn new(git_dir: &Path, rev: &str) -> Result<Self, String> {
        let path = git_dir
            .join("caos-locators")
            .join(format!("{rev}.incoming"));
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|e| format!("clearing {path:?}: {e}"))?;
        }
        fs::create_dir_all(&path).map_err(|e| format!("creating {path:?}: {e}"))?;
        Ok(Self { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "cannot remove locator staging directory {}: {error}",
                self.path.display()
            );
        }
    }
}
