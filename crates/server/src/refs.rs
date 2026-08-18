//! Exact ref reads and compare-and-append updates.
//!
//! Git's smart-HTTP advertisement is deliberately a repository-wide list. That
//! is useful for an ordinary clone, but pathological for a durable event log:
//! every event used to download every request and result ref merely to read and
//! move one known head. These endpoints keep the operation generic while making
//! its cost independent of the number of unrelated refs.

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Config, HttpError};
use serde::Deserialize;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const HOOK_MARKER: &str = "# managed by caos-server: append-only refs";
const LEGACY_HOOK_MARKER: &str = "# managed by caos-server: append-only conversation heads";

fn bash_in_path() -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").ok_or("PATH is not set; cannot install Git hook")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bash");
        let executable = std::fs::metadata(&candidate)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if executable
            && candidate
                .to_str()
                .is_some_and(|path| !path.chars().any(char::is_whitespace))
        {
            return Ok(candidate);
        }
    }
    Err("cannot find an executable bash on PATH for Git hook".to_string())
}

fn shell_word(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("hook executable path is not UTF-8: {}", path.display()))?;
    Ok(format!("'{}'", path.replace('\'', "'\"'\"'")))
}

fn pre_receive_hook() -> Result<String, String> {
    let bash = bash_in_path()?;
    let server = std::env::current_exe()
        .map_err(|error| format!("locating the server executable for Git hook: {error}"))?;
    Ok(format!(
        "#!{}\n{HOOK_MARKER}\nexec {} --validate-pre-receive\n",
        bash.display(),
        shell_word(&server)?
    ))
}

#[derive(Deserialize)]
struct ReadRequest {
    #[serde(rename = "ref")]
    refname: String,
}

#[derive(Deserialize)]
struct AppendRequest {
    #[serde(rename = "ref")]
    refname: String,
    expected: Option<String>,
    new: String,
}

#[derive(Deserialize)]
struct TransactionRequest {
    updates: Vec<ExactUpdate>,
}

#[derive(Deserialize)]
struct ExactUpdate {
    #[serde(rename = "ref")]
    refname: String,
    expected: Option<String>,
    new: Option<String>,
}

/// Validate the complete receive-pack command set from a pre-receive hook.
/// The installed hook delegates here instead of maintaining a second event
/// parser in shell, so raw Git and `/ref/append` enforce one protocol.
pub(crate) fn validate_pre_receive(git_dir: &str, input: &str) -> Result<(), HttpError> {
    for line in input.lines() {
        let mut fields = line.split_whitespace();
        let old = fields
            .next()
            .ok_or_else(|| HttpError::new(400, "pre-receive command has no old object ID"))?;
        let new = fields
            .next()
            .ok_or_else(|| HttpError::new(400, "pre-receive command has no new object ID"))?;
        let refname = fields
            .next()
            .ok_or_else(|| HttpError::new(400, "pre-receive command has no refname"))?;
        if fields.next().is_some() {
            return Err(HttpError::new(
                400,
                "pre-receive command has unexpected fields",
            ));
        }
        validate_hash(old, "old ref target")?;
        validate_hash(new, "new ref target")?;

        if refname.starts_with("refs/caos/res/") {
            return Err(HttpError::new(403, format!("{refname} is server-owned")));
        }
        if !is_append_only_ref(refname) {
            continue;
        }
        if new == ZERO_OID {
            return Err(HttpError::new(
                422,
                format!("refusing to delete append-only ref {refname}"),
            ));
        }
        validate_append(git_dir, new, if old == ZERO_OID { None } else { Some(old) })?;
    }
    Ok(())
}

/// Install the server-owned receive-pack guard before accepting requests.
/// Refuse to replace an unrelated administrator hook; previous versions of our
/// own hook are safe to upgrade in place.
pub(crate) fn install_hook(git_dir: &str) -> Result<(), String> {
    let hooks = Path::new(git_dir).join("hooks");
    let configured = Command::new("git")
        .args(["-C", git_dir, "config", "--get", "core.hooksPath"])
        .output()
        .map_err(|error| format!("reading core.hooksPath: {error}"))?;
    if configured.status.success() {
        let current = String::from_utf8_lossy(&configured.stdout);
        let current = current.trim();
        if current != "hooks" && Path::new(current) != hooks {
            return Err(format!(
                "refusing to replace configured core.hooksPath {current:?}"
            ));
        }
    } else if configured.status.code() != Some(1) {
        return Err(format!(
            "reading core.hooksPath: {}",
            String::from_utf8_lossy(&configured.stderr).trim()
        ));
    }
    std::fs::create_dir_all(&hooks)
        .map_err(|error| format!("creating {}: {error}", hooks.display()))?;
    let hook = hooks.join("pre-receive");
    match std::fs::read(&hook) {
        Ok(existing) => {
            let has_marker = |marker: &str| {
                existing
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes())
            };
            if !has_marker(HOOK_MARKER) && !has_marker(LEGACY_HOOK_MARKER) {
                return Err(format!(
                    "refusing to replace unmanaged pre-receive hook {}",
                    hook.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "reading existing pre-receive hook {}: {error}",
                hook.display(),
            ));
        }
    }
    std::fs::write(&hook, pre_receive_hook()?)
        .map_err(|error| format!("writing {}: {error}", hook.display()))?;
    let mut permissions = std::fs::metadata(&hook)
        .map_err(|error| format!("reading {} metadata: {error}", hook.display()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions)
        .map_err(|error| format!("making {} executable: {error}", hook.display()))?;
    let output = Command::new("git")
        .args([
            "-C",
            git_dir,
            "config",
            "core.hooksPath",
            hooks.to_str().ok_or("hooks path is not UTF-8")?,
        ])
        .output()
        .map_err(|error| format!("configuring core.hooksPath: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "configuring core.hooksPath: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `POST /ref/read` with `{"ref":"refs/..."}`. A missing ref is 404; a
/// present ref returns its exact object id and a newline.
pub(crate) fn read(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let request: ReadRequest = serde_json::from_str(body)
        .map_err(|error| HttpError::new(400, format!("invalid ref read: {error}")))?;
    validate_ref(&config.git_dir, &request.refname)?;
    match read_ref(&config.git_dir, &request.refname)? {
        Some(hash) => Ok(format!("{hash}\n").into_bytes()),
        None => Err(HttpError::new(404, "ref not found")),
    }
}

/// `POST /ref/append` with `{"ref":"refs/...","expected":A,"new":B}`.
/// `expected: null` creates an absent ref. An update succeeds only when the
/// current value is still `expected` and `new` reaches it on its first-parent
/// chain. The latter deliberately permits a batch such as `A -> B -> C` to be
/// admitted with one ref move from A to C.
pub(crate) fn append(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let request: AppendRequest = serde_json::from_str(body)
        .map_err(|error| HttpError::new(400, format!("invalid ref append: {error}")))?;
    validate_ref(&config.git_dir, &request.refname)?;
    if request.refname.starts_with("refs/caos/res/") {
        return Err(HttpError::new(
            403,
            format!("{} is server-owned", request.refname),
        ));
    }
    validate_hash(&request.new, "new ref target")?;
    if let Some(expected) = request.expected.as_deref() {
        validate_hash(expected, "expected ref target")?;
    }

    validate_append(&config.git_dir, &request.new, request.expected.as_deref())?;
    validate_connectivity(&config.git_dir, &request.new, request.expected.as_deref())?;

    let observed = read_ref(&config.git_dir, &request.refname)?;
    if observed.as_deref() != request.expected.as_deref() {
        // The update may have succeeded while its response was lost, and a
        // later writer may already have appended again. Candidate validation
        // above proves expected -> new; only new -> observed remains to prove
        // before treating the retry as idempotent success.
        if let Some(observed) = observed.as_deref() {
            if first_parent_contains(&config.git_dir, observed, &request.new)? {
                return Ok(format!("{}\n", request.new).into_bytes());
            }
        }
        return Err(conflict(observed.as_deref()));
    }

    let expected = request.expected.as_deref().unwrap_or(ZERO_OID);
    let output = Command::new("git")
        .args([
            "-C",
            &config.git_dir,
            "update-ref",
            "--create-reflog",
            &request.refname,
            &request.new,
            expected,
        ])
        .output()
        .map_err(|error| HttpError::new(500, format!("running git update-ref: {error}")))?;
    if output.status.success() {
        return Ok(format!("{}\n", request.new).into_bytes());
    }

    // update-ref owns the real atomic comparison. A pre-read only improves the
    // common conflict response; it never substitutes for this post-failure read.
    let now = read_ref(&config.git_dir, &request.refname)?;
    if now.as_deref() != request.expected.as_deref() {
        return Err(conflict(now.as_deref()));
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(HttpError::new(
        500,
        format!("updating {}: {}", request.refname, detail.trim()),
    ))
}

/// Atomically apply a small set of exact ref comparisons. Unlike `/ref/append`,
/// this has no ancestry policy of its own; refs whose structural name marks
/// them append-only still pass the same append validator. A null `new` verifies
/// or deletes the named ref.
pub(crate) fn transaction(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let request: TransactionRequest = serde_json::from_str(body)
        .map_err(|error| HttpError::new(400, format!("invalid ref transaction: {error}")))?;
    if request.updates.is_empty() || request.updates.len() > 64 {
        return Err(HttpError::new(400, "a ref transaction needs 1-64 updates"));
    }

    let mut names = HashSet::new();
    for update in &request.updates {
        validate_ref(&config.git_dir, &update.refname)?;
        if !names.insert(update.refname.as_str()) {
            return Err(HttpError::new(
                400,
                format!("duplicate ref in transaction: {}", update.refname),
            ));
        }
        if update.refname.starts_with("refs/caos/res/") {
            return Err(HttpError::new(
                403,
                format!("{} is server-owned", update.refname),
            ));
        }
        if let Some(expected) = update.expected.as_deref() {
            validate_hash(expected, "expected ref target")?;
        }
        if let Some(new) = update.new.as_deref() {
            validate_hash(new, "new ref target")?;
            validate_object(&config.git_dir, new)?;
        }
        if is_append_only_ref(&update.refname) {
            let new = update.new.as_deref().ok_or_else(|| {
                HttpError::new(
                    422,
                    format!("refusing to delete append-only ref {}", update.refname),
                )
            })?;
            validate_append(&config.git_dir, new, update.expected.as_deref())?;
            validate_connectivity(&config.git_dir, new, update.expected.as_deref())?;
        }
    }

    for update in &request.updates {
        let observed = read_ref(&config.git_dir, &update.refname)?;
        if observed.as_deref() != update.expected.as_deref() {
            return Err(HttpError::new(
                409,
                format!(
                    "{} changed to {}",
                    update.refname,
                    observed.as_deref().unwrap_or("absent")
                ),
            ));
        }
    }

    let mut commands = String::from("start\n");
    for update in &request.updates {
        match (update.expected.as_deref(), update.new.as_deref()) {
            (None, Some(new)) => commands.push_str(&format!("create {} {new}\n", update.refname)),
            (Some(expected), Some(new)) => {
                commands.push_str(&format!("update {} {new} {expected}\n", update.refname));
            }
            (Some(expected), None) => {
                commands.push_str(&format!("delete {} {expected}\n", update.refname));
            }
            (None, None) => {
                commands.push_str(&format!("verify {} {ZERO_OID}\n", update.refname));
            }
        }
    }
    commands.push_str("prepare\ncommit\n");

    let mut child = Command::new("git")
        .args([
            "-C",
            &config.git_dir,
            "update-ref",
            "--create-reflog",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| HttpError::new(500, format!("running git update-ref: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| HttpError::new(500, "opening git update-ref stdin"))?
        .write_all(commands.as_bytes())
        .map_err(|error| HttpError::new(500, format!("writing git update-ref stdin: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| HttpError::new(500, format!("waiting for git update-ref: {error}")))?;
    if output.status.success() {
        return Ok(b"ok\n".to_vec());
    }
    for update in &request.updates {
        if read_ref(&config.git_dir, &update.refname)?.as_deref() != update.expected.as_deref() {
            return Err(HttpError::new(
                409,
                format!("ref transaction lost a comparison at {}", update.refname),
            ));
        }
    }
    Err(HttpError::new(
        500,
        format!(
            "git update-ref transaction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
}

fn conflict(observed: Option<&str>) -> HttpError {
    HttpError::new(
        409,
        observed
            .map(|hash| format!("ref changed to {hash}"))
            .unwrap_or_else(|| "ref is absent".to_string()),
    )
}

fn validate_ref(git_dir: &str, refname: &str) -> Result<(), HttpError> {
    if !refname.starts_with("refs/") {
        return Err(HttpError::new(400, format!("invalid refname {refname:?}")));
    }
    let status = Command::new("git")
        .args(["-C", git_dir, "check-ref-format", refname])
        .status()
        .map_err(|error| HttpError::new(500, format!("running git check-ref-format: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(HttpError::new(400, format!("invalid refname {refname:?}")))
    }
}

fn validate_hash(hash: &str, what: &str) -> Result<(), HttpError> {
    if hash.len() == 40
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(HttpError::new(400, format!("invalid {what} {hash:?}")))
    }
}

fn read_ref(git_dir: &str, refname: &str) -> Result<Option<String>, HttpError> {
    let output = Command::new("git")
        .args(["-C", git_dir, "rev-parse", "--verify", "--quiet", refname])
        .output()
        .map_err(|error| HttpError::new(500, format!("running git rev-parse: {error}")))?;
    if output.status.success() {
        let hash = String::from_utf8(output.stdout)
            .map_err(|error| HttpError::new(500, format!("ref target is not UTF-8: {error}")))?;
        let hash = hash.trim();
        validate_hash(hash, "stored ref target")?;
        Ok(Some(hash.to_string()))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(HttpError::new(
            500,
            format!(
                "reading {refname}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }
}

fn validate_commit(git_dir: &str, hash: &str) -> Result<(), HttpError> {
    let output = Command::new("git")
        .args(["-C", git_dir, "cat-file", "-t", hash])
        .output()
        .map_err(|error| HttpError::new(500, format!("running git cat-file: {error}")))?;
    if output.status.success() && output.stdout == b"commit\n" {
        Ok(())
    } else {
        Err(HttpError::new(
            422,
            format!("new ref target {hash} is not a stored commit"),
        ))
    }
}

fn validate_object(git_dir: &str, hash: &str) -> Result<(), HttpError> {
    let status = Command::new("git")
        .args(["-C", git_dir, "cat-file", "-e", hash])
        .status()
        .map_err(|error| HttpError::new(500, format!("running git cat-file: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(HttpError::new(
            422,
            format!("new ref target {hash} is not stored"),
        ))
    }
}

/// Append-only logs use a structural ref convention rather than a server-owned
/// application namespace. Any `refs/caos/<path>/head` is a commit log: it may
/// be created, then advanced along first-parent history, but never reset or
/// deleted. The server does not know what the commits mean.
pub(crate) fn is_append_only_ref(refname: &str) -> bool {
    refname
        .strip_prefix("refs/caos/")
        .and_then(|path| path.strip_suffix("/head"))
        .is_some_and(|path| !path.is_empty())
}

/// Validate the storage-level contract for an append-only commit log. Commit
/// messages and trees are opaque; their application protocol belongs to the
/// client that writes and reads the log.
fn validate_append(git_dir: &str, new_hash: &str, expected: Option<&str>) -> Result<(), HttpError> {
    validate_commit(git_dir, new_hash)?;
    if let Some(expected) = expected {
        if !first_parent_contains(git_dir, new_hash, expected)? {
            return Err(HttpError::new(
                422,
                format!(
                    "new target {new_hash} does not first-parent-descend from expected {expected}"
                ),
            ));
        }
    }
    Ok(())
}

/// Verify the object closure before making it reachable. Receive-pack normally
/// performs this check while consuming a pack; the exact-ref endpoint moves an
/// already-stored object and therefore has to preserve that guarantee itself.
/// On an update, objects reachable from the trusted old tip are excluded so a
/// normal one-event append checks only the newly introduced closure.
fn validate_connectivity(
    git_dir: &str,
    new_hash: &str,
    expected: Option<&str>,
) -> Result<(), HttpError> {
    let mut args = vec![
        "-C",
        git_dir,
        "rev-list",
        "--objects",
        "--missing=print",
        new_hash,
    ];
    if let Some(expected) = expected {
        args.extend(["--not", expected]);
    }
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|error| HttpError::new(500, format!("checking object connectivity: {error}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            422,
            format!(
                "cannot walk objects for {new_hash}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let missing = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix('?'))
        .map(str::to_string);
    if let Some(missing) = missing {
        return Err(HttpError::new(
            422,
            format!("new ref target {new_hash} is missing reachable object {missing}"),
        ));
    }
    Ok(())
}

fn first_parent_contains(git_dir: &str, tip: &str, ancestor: &str) -> Result<bool, HttpError> {
    if tip == ancestor {
        return Ok(true);
    }
    let output = Command::new("git")
        .args([
            "-C",
            git_dir,
            "rev-list",
            "--first-parent",
            "--parents",
            tip,
            "--not",
            ancestor,
        ])
        .output()
        .map_err(|error| HttpError::new(500, format!("running git rev-list: {error}")))?;
    if !output.status.success() {
        return Err(HttpError::new(422, format!("cannot walk commit {tip}")));
    }
    let history = String::from_utf8(output.stdout)
        .map_err(|error| HttpError::new(500, format!("rev-list output is not UTF-8: {error}")))?;
    // In the accepted case exclusion stops the bounded first-parent walk at
    // `ancestor`, leaving it as the first parent on the last emitted line.
    // If it is reachable only through a merge's other parent, that token is a
    // different commit and the update remains a rewrite for this event log.
    Ok(history
        .lines()
        .last()
        .and_then(|line| line.split_whitespace().nth(1))
        == Some(ancestor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn git(args: &[&str]) -> String {
        let output = Command::new("git").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn commit(dir: &str, tree: &str, message: &str, parents: &[&str]) -> String {
        let mut args = vec!["-C", dir, "commit-tree", tree];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        args.extend(["-m", message]);
        let output = Command::new("git")
            .env("GIT_AUTHOR_NAME", "caos")
            .env("GIT_AUTHOR_EMAIL", "caos@caos")
            .env("GIT_COMMITTER_NAME", "caos")
            .env("GIT_COMMITTER_EMAIL", "caos@caos")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn write_object(dir: &str, kind: &str, body: &str) -> String {
        let mut child = Command::new("git")
            .args(["-C", dir, "hash-object", "-t", kind, "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn repo() -> (String, String, String, String, String) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("caos-ref-test-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let dir = dir.to_string_lossy().into_owned();
        git(&["init", "-q", "--bare", &dir]);
        let tree = git(&["-C", &dir, "mktree"]);
        let a = commit(&dir, &tree, "a", &[]);
        let b = commit(&dir, &tree, "b", &[&a]);
        let c = commit(&dir, &tree, "c", &[&b]);
        let d = commit(&dir, &tree, "d", &[&a]);
        (dir, a, b, c, d)
    }

    fn config(dir: &str) -> Config {
        Config {
            registry_push_url: String::new(),
            registry_pull_host: String::new(),
            redis_addr: String::new(),
            git_dir: dir.to_string(),
            repo: gix::open(dir).unwrap().into_sync(),
            trace: crate::trace::Hub::default(),
        }
    }

    #[test]
    fn first_parent_walk_accepts_a_batched_descendant() {
        let (dir, a, _b, c, d) = repo();
        assert_eq!(first_parent_contains(&dir, &c, &a).ok(), Some(true));
        assert_eq!(first_parent_contains(&dir, &a, &c).ok(), Some(false));
        let tree = git(&["-C", &dir, "rev-parse", &format!("{c}^{{tree}}")]);
        let merge = commit(&dir, &tree, "merge", &[&d, &c]);
        assert_eq!(first_parent_contains(&dir, &merge, &d).ok(), Some(true));
        assert_eq!(first_parent_contains(&dir, &merge, &c).ok(), Some(false));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_rejects_a_commit_with_a_missing_object() {
        let (dir, a, _b, _c, _d) = repo();
        let config = config(&dir);
        let missing_tree = "1111111111111111111111111111111111111111";
        let body = format!(
            "tree {missing_tree}\nparent {a}\nauthor caos <caos@caos> 0 +0000\ncommitter caos <caos@caos> 0 +0000\n\n{{\"base\":\"{a}\"}}\n"
        );
        let broken = write_object(&dir, "commit", &body);
        let request = serde_json::json!({
            "ref": "refs/caos/logs/broken/head",
            "expected": null,
            "new": broken,
        })
        .to_string();
        let error = append(&config, &request).err().unwrap();
        assert_eq!(error.status(), 422);
        assert!(error.message().contains(missing_tree));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_endpoint_is_atomic_batched_and_idempotent() {
        let (dir, a, b, c, d) = repo();
        let config = config(&dir);
        let refname = "refs/caos/logs/test/head";
        let request = |expected: Option<&str>, new: &str| {
            serde_json::json!({"ref": refname, "expected": expected, "new": new}).to_string()
        };

        let uppercase = "A".repeat(40);
        let error = append(&config, &request(None, &uppercase)).err().unwrap();
        assert_eq!(error.status(), 400);
        let error = append(&config, &request(Some(&uppercase), &a))
            .err()
            .unwrap();
        assert_eq!(error.status(), 400);

        if let Err(error) = append(&config, &request(None, &a)) {
            panic!("{}: {}", error.status(), error.message());
        }
        // A -> C admits the complete A -> B -> C first-parent batch.
        assert!(append(&config, &request(Some(&a), &c)).is_ok());
        assert_eq!(
            String::from_utf8(
                read(&config, &serde_json::json!({"ref": refname}).to_string())
                    .ok()
                    .unwrap()
            )
            .unwrap()
            .trim(),
            c
        );
        // Retrying an update whose response was lost is success, even though
        // its original expected value is now stale.
        assert!(append(&config, &request(Some(&a), &c)).is_ok());
        // An older candidate is not a successful retry merely because the
        // observed head happens to descend from it.
        let error = append(&config, &request(Some(&b), &a)).unwrap_err();
        assert_eq!(error.status(), 422);
        let error = append(&config, &request(Some(&a), &d)).err().unwrap();
        assert_eq!(error.status(), 409);
        let error = append(&config, &request(Some(&c), &b)).err().unwrap();
        assert_eq!(error.status(), 422);
        let result_ref = serde_json::json!({
            "ref": "refs/caos/res/request",
            "expected": null,
            "new": a,
        })
        .to_string();
        let error = append(&config, &result_ref).err().unwrap();
        assert_eq!(error.status(), 403);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exact_ref_transaction_creates_an_append_log_and_indexes_together() {
        let (dir, root, _b, _c, _d) = repo();
        let config = config(&dir);
        let title = write_object(&dir, "blob", "Subagent");
        let head_ref = "refs/caos/v2/conversations/agent/head";
        let title_ref = "refs/caos/v2/conversations/agent/title";
        let active_ref = "refs/caos/v2/users/u-owner/conversations/active/c-agent";
        let archived_ref = "refs/caos/v2/users/u-owner/conversations/archived/c-agent";
        let request = serde_json::json!({
            "updates": [
                {"ref": head_ref, "expected": null, "new": root},
                {"ref": title_ref, "expected": null, "new": title},
                {"ref": active_ref, "expected": null, "new": root},
                {"ref": archived_ref, "expected": null, "new": null},
            ]
        })
        .to_string();

        assert!(transaction(&config, &request).is_ok());
        assert_eq!(
            read_ref(&dir, head_ref).ok().unwrap().as_deref(),
            Some(root.as_str())
        );
        assert_eq!(
            read_ref(&dir, title_ref).ok().unwrap().as_deref(),
            Some(title.as_str())
        );
        assert_eq!(
            read_ref(&dir, active_ref).ok().unwrap().as_deref(),
            Some(root.as_str())
        );
        assert_eq!(read_ref(&dir, archived_ref).ok().unwrap(), None);

        let loser_ref = "refs/caos/v2/conversations/loser/title";
        let loser = serde_json::json!({
            "updates": [
                {"ref": loser_ref, "expected": null, "new": title},
                {"ref": head_ref, "expected": null, "new": root},
            ]
        })
        .to_string();
        assert_eq!(transaction(&config, &loser).unwrap_err().status(), 409);
        assert_eq!(read_ref(&dir, loser_ref).ok().unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exact_ref_transaction_protects_any_caos_head() {
        let (dir, a, b, c, _d) = repo();
        let config = config(&dir);
        let refname = "refs/caos/another/protocol/head";
        let request = |expected: Option<&str>, new: Option<&str>| {
            serde_json::json!({
                "updates": [{"ref": refname, "expected": expected, "new": new}],
            })
            .to_string()
        };

        assert!(transaction(&config, &request(None, Some(&a))).is_ok());
        assert!(transaction(&config, &request(Some(&a), Some(&c))).is_ok());

        let rewrite = transaction(&config, &request(Some(&c), Some(&b))).unwrap_err();
        assert_eq!(rewrite.status(), 422);
        let deletion = transaction(&config, &request(Some(&c), None)).unwrap_err();
        assert_eq!(deletion.status(), 422);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pre_receive_uses_the_same_generic_append_validator_as_exact_append() {
        let (dir, a, b, c, _d) = repo();
        let refname = "refs/caos/logs/test/head";
        let command = |old: &str, new: &str, target: &str| format!("{old} {new} {target}\n");

        // A creation may publish a whole commit spine, and an update may
        // likewise admit a batch that reaches its exact old head.
        assert!(validate_pre_receive(&dir, &command(ZERO_OID, &c, refname)).is_ok());
        assert!(validate_pre_receive(&dir, &command(&a, &c, refname)).is_ok());

        let tree = git(&["-C", &dir, "rev-parse", &format!("{c}^{{tree}}")]);
        for message in ["opaque", "{}\n{}", r#"{"v":2}"#, r#"{"number":01}"#] {
            let opaque = commit(&dir, &tree, message, &[&c]);
            assert!(
                validate_pre_receive(&dir, &command(&c, &opaque, refname)).is_ok(),
                "server interpreted opaque commit message {message:?}"
            );
        }

        // A rewrite cannot claim an older candidate as a successful retry.
        assert!(validate_pre_receive(&dir, &command(&c, &b, refname)).is_err());
        assert!(validate_pre_receive(&dir, &command(&c, ZERO_OID, refname)).is_err());
        assert!(validate_pre_receive(&dir, &command(&a, &b, "refs/caos/res/request"),).is_err());

        assert!(is_append_only_ref("refs/caos/v2/conversations/test/head"));
        assert!(is_append_only_ref("refs/caos/another/protocol/head"));
        assert!(!is_append_only_ref("refs/caos/v2/conversations/test/title"));
        assert!(!is_append_only_ref("refs/heads/main"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_treats_commit_messages_as_opaque() {
        let (dir, root, _b, _c, _d) = repo();
        let config = config(&dir);
        let refname = "refs/caos/logs/opaque/head";
        let request = |expected: Option<&str>, new: &str| {
            serde_json::json!({"ref": refname, "expected": expected, "new": new}).to_string()
        };
        if let Err(error) = append(&config, &request(None, &root)) {
            panic!("{}: {}", error.status(), error.message());
        }
        let tree = git(&["-C", &dir, "rev-parse", &format!("{root}^{{tree}}")]);
        let mut parent = root;
        for message in [
            r#"{"v":2,"status":"running"}"#,
            r#"{"base":"not-a-hash"}"#,
            r#"{"forked_from":"also-not-a-hash"}"#,
            "{}\n{}",
        ] {
            let next = commit(&dir, &tree, message, &[&parent]);
            assert!(append(&config, &request(Some(&parent), &next)).is_ok());
            parent = next;
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hook_installation_refuses_an_unmanaged_non_utf8_hook() {
        let (dir, _a, _b, _c, _d) = repo();
        let hook = Path::new(&dir).join("hooks/pre-receive");
        std::fs::write(&hook, [0xff, 0xfe]).unwrap();
        let error = install_hook(&dir).unwrap_err();
        assert!(error.contains("unmanaged pre-receive hook"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn managed_hook_upgrade_delegates_to_the_shared_validator() {
        if bash_in_path().is_err() {
            return;
        }
        let (dir, _a, _b, _c, _d) = repo();
        let hook = Path::new(&dir).join("hooks/pre-receive");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\n{LEGACY_HOOK_MARKER}\ncase refs/caos/conversations/test/head in\n  refs/caos/conversations/*/head) ;;\nesac\n"
            ),
        )
        .unwrap();

        install_hook(&dir).unwrap();

        let installed = std::fs::read_to_string(&hook).unwrap();
        assert!(installed.contains(HOOK_MARKER));
        assert!(!installed.contains(LEGACY_HOOK_MARKER));
        assert!(installed.contains("--validate-pre-receive"));
        assert!(!installed.contains("jq"));
        assert!(!installed.contains("refs/caos/conversations/*/head"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
