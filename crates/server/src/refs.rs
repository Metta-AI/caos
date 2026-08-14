//! Exact ref reads and compare-and-append updates.
//!
//! Git's smart-HTTP advertisement is deliberately a repository-wide list. That
//! is useful for an ordinary clone, but pathological for a durable event log:
//! every event used to download every request and result ref merely to read and
//! move one known head. These endpoints keep the operation generic while making
//! its cost independent of the number of unrelated refs.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::{Config, HttpError};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const HOOK_MARKER: &str = "# managed by caos-server: append-only conversation heads";

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
        if !is_conversation_head_ref(refname) {
            continue;
        }
        if new == ZERO_OID {
            return Err(HttpError::new(
                422,
                format!("refusing to delete append-only ref {refname}"),
            ));
        }
        validate_conversation_append(git_dir, new, if old == ZERO_OID { None } else { Some(old) })?;
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
            if !existing
                .windows(HOOK_MARKER.len())
                .any(|window| window == HOOK_MARKER.as_bytes())
            {
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

    validate_commit(&config.git_dir, &request.new)?;
    if is_conversation_head_ref(&request.refname) {
        validate_conversation_append(&config.git_dir, &request.new, request.expected.as_deref())?;
    } else if let Some(expected) = request.expected.as_deref() {
        if !first_parent_contains(&config.git_dir, &request.new, expected)? {
            return Err(HttpError::new(
                422,
                format!(
                    "new target {} does not first-parent-descend from expected {expected}",
                    request.new
                ),
            ));
        }
    }
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

fn is_conversation_head_ref(refname: &str) -> bool {
    refname
        .strip_prefix("refs/caos/v2/conversations/")
        .and_then(|rest| rest.strip_suffix("/head"))
        .is_some_and(|conversation| !conversation.is_empty())
}

struct StoredConversationEvent {
    first_parent: String,
    value: Value,
}

fn read_conversation_event(
    git_dir: &str,
    hash: &str,
) -> Result<StoredConversationEvent, HttpError> {
    let output = Command::new("git")
        .args(["-C", git_dir, "cat-file", "commit", hash])
        .output()
        .map_err(|error| HttpError::new(500, format!("reading commit {hash}: {error}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            422,
            format!("conversation event {hash} is not a stored commit"),
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|error| {
        HttpError::new(
            422,
            format!("conversation event commit {hash} is not UTF-8: {error}"),
        )
    })?;
    let (headers, message) = text.split_once("\n\n").ok_or_else(|| {
        HttpError::new(
            422,
            format!("conversation event commit {hash} has no message"),
        )
    })?;
    let first_parent = headers
        .lines()
        .find_map(|line| line.strip_prefix("parent "))
        .ok_or_else(|| {
            HttpError::new(
                422,
                format!("conversation event commit {hash} has no first parent"),
            )
        })?;
    validate_hash(first_parent, "conversation event parent")?;
    let value = serde_json::from_str::<Value>(message.trim()).map_err(|error| {
        HttpError::new(
            422,
            format!("conversation event commit {hash} is not JSON: {error}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        HttpError::new(
            422,
            format!("conversation event commit {hash} must contain a JSON object"),
        )
    })?;
    if object.contains_key("v") {
        return Err(HttpError::new(
            422,
            format!(
                "conversation event commit {hash} must not carry a version; refs/caos/v2 selects the protocol"
            ),
        ));
    }
    Ok(StoredConversationEvent {
        first_parent: first_parent.to_string(),
        value,
    })
}

fn validate_declared_base(event: &StoredConversationEvent) -> Result<bool, HttpError> {
    let Some(base) = event.value.get("base") else {
        return Ok(false);
    };
    let base = base
        .as_str()
        .ok_or_else(|| HttpError::new(422, "conversation event base must be a string"))?;
    validate_hash(base, "conversation base")?;
    if base != event.first_parent {
        return Err(HttpError::new(
            422,
            format!(
                "conversation root declares base {base}, but its first parent is {}",
                event.first_parent
            ),
        ));
    }
    Ok(true)
}

fn validate_declared_fork(event: &StoredConversationEvent) -> Result<bool, HttpError> {
    let Some(forked_from) = event.value.get("forked_from") else {
        return Ok(false);
    };
    if event.value.get("base").is_some() {
        return Err(HttpError::new(
            422,
            "a conversation fork marker must not introduce a new base",
        ));
    }
    let forked_from = forked_from
        .as_str()
        .ok_or_else(|| HttpError::new(422, "conversation event forked_from must be a string"))?;
    validate_hash(forked_from, "forked_from")?;
    if forked_from != event.first_parent {
        return Err(HttpError::new(
            422,
            format!(
                "conversation fork marker declares {forked_from}, but its first parent is {}",
                event.first_parent
            ),
        ));
    }
    Ok(true)
}

/// Validate only the small event envelope needed to preserve the first-parent
/// boundary. Transcript and run semantics remain client/worker concerns.
fn validate_conversation_append(
    git_dir: &str,
    new_hash: &str,
    expected: Option<&str>,
) -> Result<(), HttpError> {
    let mut current = new_hash.to_string();
    loop {
        if expected == Some(current.as_str()) {
            return Ok(());
        }
        let event = read_conversation_event(git_dir, &current)?;
        let is_root = validate_declared_base(&event)?;
        let is_fork = validate_declared_fork(&event)?;
        if expected.is_some() && is_root {
            return Err(HttpError::new(
                422,
                "an update must not introduce a second conversation base",
            ));
        }
        if expected.is_some() && is_fork {
            return Err(HttpError::new(
                422,
                "an update must not introduce a conversation fork marker",
            ));
        }
        if expected.is_none() && is_root {
            return Ok(());
        }
        current = event.first_parent;
    }
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

    fn chat_repo() -> (String, String, String, String, String, String) {
        let (dir, base, _plain_b, _plain_c, _plain_d) = repo();
        let tree = git(&["-C", &dir, "rev-parse", &format!("{base}^{{tree}}")]);
        let root = commit(
            &dir,
            &tree,
            &serde_json::json!({"base": base, "status": "idle"}).to_string(),
            &[&base],
        );
        let b = commit(&dir, &tree, r#"{"status":"running"}"#, &[&root]);
        let c = commit(&dir, &tree, r#"{"status":"idle"}"#, &[&b]);
        let d = commit(&dir, &tree, r#"{"status":"failed"}"#, &[&root]);
        (dir, base, root, b, c, d)
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
            "ref": "refs/caos/v2/conversations/broken/head",
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
        let (dir, a, root, b, c, d) = chat_repo();
        let config = config(&dir);
        let refname = "refs/caos/v2/conversations/test/head";
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

        if let Err(error) = append(&config, &request(None, &root)) {
            panic!("{}: {}", error.status(), error.message());
        }
        // Root -> C admits the complete Root -> B -> C first-parent batch.
        assert!(append(&config, &request(Some(&root), &c)).is_ok());
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
        assert!(append(&config, &request(Some(&root), &c)).is_ok());
        // An older candidate is not a successful retry merely because the
        // observed head happens to descend from it.
        let error = append(&config, &request(Some(&b), &root)).unwrap_err();
        assert_eq!(error.status(), 422);
        let error = append(&config, &request(Some(&root), &d)).err().unwrap();
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
    fn pre_receive_uses_the_same_event_validator_as_exact_append() {
        let (dir, base, root, b, c, _d) = chat_repo();
        let refname = "refs/caos/v2/conversations/test/head";
        let command = |old: &str, new: &str, target: &str| format!("{old} {new} {target}\n");

        // A creation may publish a whole validated event spine, and an update
        // may likewise admit a batch that reaches its exact old head.
        assert!(validate_pre_receive(&dir, &command(ZERO_OID, &c, refname)).is_ok());
        assert!(validate_pre_receive(&dir, &command(&root, &c, refname)).is_ok());

        let tree = git(&["-C", &dir, "rev-parse", &format!("{c}^{{tree}}")]);
        let versioned = commit(&dir, &tree, r#"{"v":2}"#, &[&c]);
        assert!(validate_pre_receive(&dir, &command(&c, &versioned, refname)).is_err());

        let second_base = commit(
            &dir,
            &tree,
            &serde_json::json!({"base": c}).to_string(),
            &[&c],
        );
        assert!(validate_pre_receive(&dir, &command(&c, &second_base, refname)).is_err());

        let fork_update = commit(
            &dir,
            &tree,
            &serde_json::json!({"forked_from": c}).to_string(),
            &[&c],
        );
        assert!(validate_pre_receive(&dir, &command(&c, &fork_update, refname)).is_err());

        let fork_root = commit(
            &dir,
            &tree,
            &serde_json::json!({"base": base, "forked_from": base}).to_string(),
            &[&base],
        );
        assert!(validate_pre_receive(
            &dir,
            &command(
                ZERO_OID,
                &fork_root,
                "refs/caos/v2/conversations/bad-fork/head",
            ),
        )
        .is_err());

        for message in ["{}\n{}", r#"{"base":""}"#, r#"{"number":01}"#] {
            let malformed = commit(&dir, &tree, message, &[&c]);
            assert!(
                validate_pre_receive(&dir, &command(&c, &malformed, refname)).is_err(),
                "accepted malformed event {message:?}"
            );
        }

        // A rewrite cannot claim an older candidate as a successful retry.
        assert!(validate_pre_receive(&dir, &command(&c, &b, refname)).is_err());
        assert!(validate_pre_receive(&dir, &command(&c, ZERO_OID, refname)).is_err());
        assert!(
            validate_pre_receive(&dir, &command(&base, &root, "refs/caos/res/request"),).is_err()
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_rejects_invalid_event_envelopes_and_boundaries() {
        let (dir, base, root, _b, _c, _d) = chat_repo();
        let config = config(&dir);
        let refname = "refs/caos/v2/conversations/test/head";
        let request = |expected: Option<&str>, new: &str| {
            serde_json::json!({"ref": refname, "expected": expected, "new": new}).to_string()
        };
        if let Err(error) = append(&config, &request(None, &root)) {
            panic!("{}: {}", error.status(), error.message());
        }
        let tree = git(&["-C", &dir, "rev-parse", &format!("{root}^{{tree}}")]);

        let versioned = commit(&dir, &tree, r#"{"v":2,"status":"running"}"#, &[&root]);
        let error = append(&config, &request(Some(&root), &versioned)).unwrap_err();
        assert_eq!(error.status(), 422);
        assert!(error.message().contains("must not carry a version"));

        let second_base = commit(
            &dir,
            &tree,
            &serde_json::json!({"base": root, "status": "idle"}).to_string(),
            &[&root],
        );
        let error = append(&config, &request(Some(&root), &second_base)).unwrap_err();
        assert_eq!(error.status(), 422);
        assert!(error.message().contains("second conversation base"));

        let fork_update = commit(
            &dir,
            &tree,
            &serde_json::json!({"forked_from": root}).to_string(),
            &[&root],
        );
        let error = append(&config, &request(Some(&root), &fork_update)).unwrap_err();
        assert_eq!(error.status(), 422);
        assert!(error.message().contains("fork marker"));

        let fork_root = commit(
            &dir,
            &tree,
            &serde_json::json!({"base": base, "forked_from": base}).to_string(),
            &[&base],
        );
        let error = append(&config, &request(None, &fork_root)).unwrap_err();
        assert_eq!(error.status(), 422);
        assert!(error.message().contains("must not introduce a new base"));
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
                "#!/bin/sh\n{HOOK_MARKER}\ncase refs/caos/conversations/test/head in\n  refs/caos/conversations/*/head) ;;\nesac\n"
            ),
        )
        .unwrap();

        install_hook(&dir).unwrap();

        let installed = std::fs::read_to_string(&hook).unwrap();
        assert!(installed.contains(HOOK_MARKER));
        assert!(installed.contains("--validate-pre-receive"));
        assert!(!installed.contains("jq"));
        assert!(!installed.contains("refs/caos/conversations/*/head"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
