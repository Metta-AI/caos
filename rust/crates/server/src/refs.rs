//! Named, mutable refs that a worker reads and moves: `GET /ref`, `POST /ref`,
//! and the wait behind `caos ref-then` (design/refs.md).
//!
//! Everything else in the store is addressed by content. These are the one
//! deliberate exception a worker is offered, for state that has to outlive a
//! request (std/actor keeps its state on one) and for jobs that have to
//! signal each other (tests/sub-run). They are ordinary git refs in the
//! server's repository, so `git ls-remote` and a receive-pack push see the
//! same values.
//!
//! ONLY `refs/heads/` IS OFFERED. The rest of the namespace is the server's:
//! `refs/keys/` is the secret store, `refs/caos/res/` pins results and
//! `refs/caos/seed` once named the seed tree. A worker moving one of those
//! would be corrupting the server, not using a feature, so the endpoints
//! refuse them by name. Git requires a ref under `refs/heads/` to name a
//! commit, so the values are commits (`caos put-commit` mints one).
//!
//! A WRITE IS A COMPARE-AND-SWAP unless the caller says otherwise: it names
//! the value it expects to replace (`absent` for create-only), and git checks
//! that under the ref's lock. A stale expectation is a 409 naming the value
//! the ref actually has, so a caller that lost a race knows what it lost to.
//!
//! A WAIT HOLDS A SERVER THREAD, NOT A RUNNER SLOT. `ref-then` is a
//! continuation: the worker records it and exits, and the server's promise
//! interpreter blocks here until the ref moves away from the value the worker
//! saw. A write through `POST /ref` wakes waiters at once; a write that went
//! round this module (a `git push` to the same repository) is noticed by the
//! once-a-second recheck.

use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Config, HttpError};

/// The one namespace a worker may read and write.
pub(crate) const REF_PREFIX: &str = "refs/heads/";

/// How a missing ref is spelled, in both directions: what `GET /ref` answers
/// for one, and what a caller passes to mean "I expect there to be none" or
/// "delete it". The same word `caos push-git --expected=absent` uses.
pub(crate) const ABSENT: &str = "absent";

/// Git's spelling of "no old value" on an `update-ref` command line.
const ZEROS: &str = "0000000000000000000000000000000000000000";

/// The longest a `ref-then` may wait. A day: long enough for any human-paced
/// handshake, short enough that a forgotten waiter does not hold a server
/// thread for ever.
pub(crate) const MAX_WAIT_SECS: u64 = 24 * 60 * 60;

/// How often a waiter rereads the ref when nothing has woken it. Only a write
/// that bypassed [`set`] needs this; one that came through it wakes waiters
/// immediately.
const RECHECK: Duration = Duration::from_secs(1);

/// Bumped on every write through [`set`], so a waiter can sleep until
/// something has changed rather than rereading on a timer.
static GENERATION: Mutex<u64> = Mutex::new(0);
static CHANGED: Condvar = Condvar::new();

/// What a ref holds: nothing, or an object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Absent,
    Oid(String),
}

impl Value {
    /// Parse [`ABSENT`] or a lowercase 40-character hash. The all-zeros hash is
    /// refused: it is git's private spelling of "none", and accepting it would
    /// give "absent" two names.
    pub(crate) fn parse(text: &str) -> Result<Value, String> {
        let text = text.trim();
        if text == ABSENT {
            return Ok(Value::Absent);
        }
        if text.len() == 40
            && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && text != ZEROS
        {
            return Ok(Value::Oid(text.to_string()));
        }
        Err(format!(
            "expected a lowercase 40-character hash or {ABSENT:?}, got {text:?}"
        ))
    }

    /// The value as `GET /ref` prints it.
    pub(crate) fn text(&self) -> &str {
        match self {
            Value::Absent => ABSENT,
            Value::Oid(oid) => oid,
        }
    }
}

/// What a write requires the ref to hold first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expected {
    /// Anything: an unconditional write (`caos ref-set --force`).
    Any,
    /// Exactly this, or the write is refused with a 409.
    Is(Value),
}

/// Refuse a name outside [`REF_PREFIX`], or one git would not accept.
pub(crate) fn validate_name(name: &str) -> Result<(), HttpError> {
    if !name.starts_with(REF_PREFIX) || name.len() == REF_PREFIX.len() {
        return Err(HttpError::new(
            400,
            format!(
                "ref {name:?} must be under {REF_PREFIX}: the rest of the namespace is the \
                 server's own (refs/keys holds the secret store)"
            ),
        ));
    }
    let ok = Command::new("git")
        .args(["check-ref-format", name])
        .status()
        .map_err(|e| HttpError::new(500, format!("running git check-ref-format: {e}")))?
        .success();
    if !ok {
        return Err(HttpError::new(
            400,
            format!("{name:?} is not a valid ref name (git check-ref-format refuses it)"),
        ));
    }
    Ok(())
}

/// The ref's current value.
///
/// `for-each-ref` rather than `show-ref --verify` or `rev-parse`: it answers a
/// missing ref with no output and success, so ABSENT and BROKEN cannot be
/// confused (AGENTS.md), and it never guesses at a short name. Its pattern
/// also matches refs BELOW the name, which is why the output is filtered for
/// the exact one.
pub(crate) fn read(git_dir: &str, name: &str) -> Result<Value, HttpError> {
    let output = Command::new("git")
        .args([
            "-C",
            git_dir,
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            name,
        ])
        .output()
        .map_err(|e| HttpError::new(500, format!("running git for-each-ref: {e}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            500,
            format!(
                "reading {name}: git for-each-ref exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((oid, refname)) = line.split_once(' ') {
            if refname == name {
                return Value::parse(oid).map_err(|e| HttpError::new(500, format!("{name}: {e}")));
            }
        }
    }
    Ok(Value::Absent)
}

/// Move `name` to `new` (deleting it for [`Value::Absent`]) if it holds what
/// `expected` says. A stale expectation is a 409 that names the current value.
pub(crate) fn set(
    git_dir: &str,
    name: &str,
    new: &Value,
    expected: &Expected,
) -> Result<(), HttpError> {
    if let Value::Oid(oid) = new {
        let kind = object_kind(git_dir, oid)?;
        if kind != "commit" {
            return Err(HttpError::new(
                400,
                format!(
                    "{oid} is a {kind}: a ref under {REF_PREFIX} names a commit \
                     (`caos put-commit` mints one)"
                ),
            ));
        }
    }

    // Deleting a ref that is already gone needs no git at all, and `update-ref
    // -d` has no way to say "expect it to be absent".
    if *new == Value::Absent {
        let current = read(git_dir, name)?;
        if current == Value::Absent {
            return match expected {
                Expected::Is(Value::Oid(want)) => Err(conflict(name, &current, want)),
                _ => Ok(()),
            };
        }
        if *expected == Expected::Is(Value::Absent) {
            return Err(conflict(name, &current, ABSENT));
        }
    }

    let old = match expected {
        Expected::Any => None,
        Expected::Is(Value::Absent) => Some(ZEROS.to_string()),
        Expected::Is(Value::Oid(oid)) => Some(oid.clone()),
    };
    let mut args: Vec<String> = vec!["-C".into(), git_dir.into(), "update-ref".into()];
    match new {
        Value::Absent => args.extend(["-d".into(), name.into()]),
        Value::Oid(oid) => args.extend([name.into(), oid.clone()]),
    }
    args.extend(old);

    // A concurrent writer holding the ref's lock makes update-ref fail at once
    // ("Unable to create ... .lock: File exists"). That is not a lost race —
    // nothing has been compared yet — so wait it out briefly, as
    // `pin_result_in` does, and only then decide what the failure means.
    const ATTEMPTS: usize = 8;
    let mut stderr = String::new();
    for attempt in 0..ATTEMPTS {
        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| HttpError::new(500, format!("running git update-ref: {e}")))?;
        if output.status.success() {
            notify();
            return Ok(());
        }
        stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.contains(".lock") {
            break;
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Refused. If the ref no longer holds what the caller expected, that is
    // the answer whatever git's wording; otherwise it is a real failure.
    if let Expected::Is(want) = expected {
        let current = read(git_dir, name)?;
        if current != *want {
            return Err(conflict(name, &current, want.text()));
        }
    }
    Err(HttpError::new(
        500,
        format!(
            "moving {name} to {}: git update-ref failed: {stderr}",
            new.text()
        ),
    ))
}

fn conflict(name: &str, current: &Value, expected: &str) -> HttpError {
    HttpError::new(
        409,
        format!("ref {name} is {}, not {expected}", current.text()),
    )
}

fn object_kind(git_dir: &str, oid: &str) -> Result<String, HttpError> {
    let output = Command::new("git")
        .args(["-C", git_dir, "cat-file", "-t", oid])
        .output()
        .map_err(|e| HttpError::new(500, format!("running git cat-file: {e}")))?;
    if !output.status.success() {
        return Err(HttpError::new(
            400,
            format!("{oid} is not in the store: put it before pointing a ref at it"),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Wake every waiter: something may have changed.
fn notify() {
    let mut generation = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    *generation = generation.wrapping_add(1);
    CHANGED.notify_all();
}

/// Block until `name` holds something other than `from`, and return what it
/// holds; fail once `timeout` has passed with it unchanged.
///
/// The generation is taken BEFORE each read, so a write landing between the
/// read and the sleep moves it and the sleep returns at once: there is no
/// window in which a change can be missed until the next recheck.
pub(crate) fn wait_for_change(
    git_dir: &str,
    name: &str,
    from: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let seen = *GENERATION.lock().unwrap_or_else(|e| e.into_inner());
        let now = read(git_dir, name).map_err(|e| e.message().to_string())?;
        if now != *from {
            return Ok(now);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "ref {name} was still {} after {}s",
                from.text(),
                timeout.as_secs()
            ));
        }
        let guard = GENERATION.lock().unwrap_or_else(|e| e.into_inner());
        let _ = CHANGED
            .wait_timeout_while(guard, left.min(RECHECK), |generation| *generation == seen)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// `GET /ref?name=<ref>` — the ref's value, `<oid>` or `absent`, and a newline.
pub(crate) fn get_endpoint(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let name = crate::compute::query_param(query, "name")
        .ok_or_else(|| HttpError::new(400, "GET /ref needs ?name=<ref>"))?;
    validate_name(&name)?;
    Ok(format!("{}\n", read(&config.git_dir, &name)?.text()).into_bytes())
}

/// `POST /ref` with `{"name": <ref>, "value": <oid|absent>, "expected":
/// <oid|absent>}` — a compare-and-swap. Leaving `expected` out makes the write
/// unconditional; the CLI asks for `--force` to say so.
pub(crate) fn set_endpoint(config: &Config, body: &str) -> Result<Vec<u8>, HttpError> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HttpError::new(400, format!("POST /ref wants a JSON object: {e}")))?;
    let field = |key: &str| json.get(key).and_then(|v| v.as_str());
    let name = field("name").ok_or_else(|| HttpError::new(400, "POST /ref needs \"name\""))?;
    validate_name(name)?;
    let value = field("value").ok_or_else(|| HttpError::new(400, "POST /ref needs \"value\""))?;
    let new = Value::parse(value).map_err(|e| HttpError::new(400, format!("value: {e}")))?;
    let expected = match json.get("expected") {
        None | Some(serde_json::Value::Null) => Expected::Any,
        Some(serde_json::Value::String(text)) => Expected::Is(
            Value::parse(text).map_err(|e| HttpError::new(400, format!("expected: {e}")))?,
        ),
        Some(other) => {
            return Err(HttpError::new(
                400,
                format!("expected must be a string, got {other}"),
            ))
        }
    };
    set(&config.git_dir, name, &new, &expected)?;
    Ok(format!("{}\n", new.text()).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare repository holding one empty-tree commit per label.
    struct Repo {
        dir: std::path::PathBuf,
    }

    impl Repo {
        fn new(tag: &str) -> Repo {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("caos-refs-{tag}-{}-{nanos}", std::process::id()));
            let status = Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&dir)
                .status()
                .unwrap();
            assert!(status.success());
            Repo { dir }
        }

        fn path(&self) -> &str {
            self.dir.to_str().unwrap()
        }

        fn git(&self, args: &[&str], stdin: &str) -> String {
            use std::io::Write;
            let mut child = Command::new("git")
                .args(["-C", self.path()])
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@caos")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@caos")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "git {args:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        fn commit(&self, label: &str) -> Value {
            let tree = self.git(&["mktree"], "");
            Value::Oid(self.git(&["commit-tree", &tree], label))
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const REF: &str = "refs/heads/tests/refs/x";

    #[test]
    fn a_missing_ref_reads_as_absent_and_a_ref_below_it_does_not_count() {
        let repo = Repo::new("absent");
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
        let c = repo.commit("below");
        set(repo.path(), &format!("{REF}y"), &c, &Expected::Any).unwrap();
        set(
            repo.path(),
            "refs/heads/tests/refs/x2/y",
            &c,
            &Expected::Any,
        )
        .unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
    }

    #[test]
    fn create_only_and_compare_and_swap() {
        let repo = Repo::new("cas");
        let (a, b) = (repo.commit("a"), repo.commit("b"));
        let absent = Expected::Is(Value::Absent);

        set(repo.path(), REF, &a, &absent).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), a);

        // Create-only again: the ref exists now, and the 409 says what it holds.
        let lost = set(repo.path(), REF, &b, &absent).unwrap_err();
        assert_eq!(lost.status, 409);
        assert!(lost.message().contains(a.text()), "{}", lost.message());

        // A stale expectation is refused and moves nothing.
        let stale = set(repo.path(), REF, &b, &Expected::Is(b.clone())).unwrap_err();
        assert_eq!(stale.status, 409);
        assert_eq!(read(repo.path(), REF).unwrap(), a);

        set(repo.path(), REF, &b, &Expected::Is(a.clone())).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), b);

        // Delete, compare-and-swap style, then again: already gone is fine
        // unless a value was expected.
        set(repo.path(), REF, &Value::Absent, &Expected::Is(b.clone())).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), Value::Absent);
        set(repo.path(), REF, &Value::Absent, &absent).unwrap();
        assert_eq!(
            set(repo.path(), REF, &Value::Absent, &Expected::Is(b.clone()))
                .unwrap_err()
                .status,
            409
        );

        // --force overwrites whatever is there.
        set(repo.path(), REF, &a, &Expected::Any).unwrap();
        set(repo.path(), REF, &b, &Expected::Any).unwrap();
        assert_eq!(read(repo.path(), REF).unwrap(), b);
    }

    #[test]
    fn a_ref_names_a_stored_commit() {
        let repo = Repo::new("kinds");
        let tree = repo.git(&["mktree"], "");
        let not_commit = set(repo.path(), REF, &Value::Oid(tree), &Expected::Any).unwrap_err();
        assert_eq!(not_commit.status, 400);
        assert!(
            not_commit.message().contains("is a tree"),
            "{}",
            not_commit.message()
        );

        let missing = Value::Oid("1".repeat(40));
        assert_eq!(
            set(repo.path(), REF, &missing, &Expected::Any).unwrap_err().status,
            400
        );
    }

    #[test]
    fn only_refs_heads_is_offered() {
        for name in ["refs/keys/abc", "refs/caos/res/abc", "HEAD", "refs/heads/"] {
            assert_eq!(validate_name(name).unwrap_err().status, 400, "{name}");
        }
        assert_eq!(validate_name("refs/heads/a..b").unwrap_err().status, 400);
        validate_name("refs/heads/actors/counter").unwrap();
    }

    #[test]
    fn values_parse_strictly() {
        assert_eq!(Value::parse(" absent\n").unwrap(), Value::Absent);
        assert!(Value::parse(&"0".repeat(40)).is_err());
        assert!(Value::parse(&"A".repeat(40)).is_err());
        assert!(Value::parse("abc").is_err());
    }

    #[test]
    fn a_wait_returns_when_a_write_lands_and_times_out_when_none_does() {
        let repo = Repo::new("wait");
        let a = repo.commit("a");

        let timed_out =
            wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_millis(200))
                .unwrap_err();
        assert!(timed_out.contains("was still absent"), "{timed_out}");

        let started = Instant::now();
        let path = repo.path().to_string();
        let writer = {
            let a = a.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                set(&path, REF, &a, &Expected::Is(Value::Absent)).unwrap();
            })
        };
        let seen = wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_secs(30))
            .unwrap();
        writer.join().unwrap();
        assert_eq!(seen, a);
        // Woken by the write, not by the once-a-second recheck.
        assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());

        // Already different: no wait at all.
        assert_eq!(
            wait_for_change(repo.path(), REF, &Value::Absent, Duration::from_secs(30)).unwrap(),
            a
        );
    }
}
