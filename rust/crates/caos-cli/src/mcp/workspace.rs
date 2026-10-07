//! Where a session's checkout is, decided once and then only READ.
//!
//! A hook is a new process every time, and its working directory is not
//! contractually the project. A resumed cloud session once came up with a cwd
//! that was no repository, and every later hook discovered nothing. So the
//! FIRST prompt of a session discovers the checkout (`$CLAUDE_PROJECT_DIR`, then
//! cwd) and records it here, keyed by session id; every later hook and tool call
//! opens THAT checkout and consults nothing else.
//!
//! There is no fallback. A missing entry, or one whose checkout is gone, is an
//! error that names the session and the path: a new container has an empty
//! cache, which is the case this exists to make noticeable rather than to paper
//! over.

use std::path::{Path, PathBuf};

use caos::GitTransport;

fn base() -> Result<PathBuf, String> {
    let home =
        std::env::var_os("HOME").ok_or("HOME is not set, so no workspace can be recorded")?;
    Ok(PathBuf::from(home).join(".cache/caos/workspaces"))
}

/// The session id is already validated as a ref component by every caller that
/// derives a conversation from it; the file name additionally cannot hold a `/`.
fn entry_in(base: &Path, session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    base.join(format!("{name}.json"))
}

pub fn exists(session: &str) -> Result<bool, String> {
    Ok(entry_in(&base()?, session).exists())
}

/// Record `t`'s checkout as the one `session` uses from now on.
pub fn record(session: &str, t: &GitTransport) -> Result<(), String> {
    record_in(&base()?, session, t.work_dir())
}

fn record_in(base: &Path, session: &str, checkout: &Path) -> Result<(), String> {
    std::fs::create_dir_all(base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let path = entry_in(base, session);
    let body = serde_json::json!({ "session": session, "checkout": checkout });
    // Temp and rename, so a reader never sees half an entry.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body.to_string())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Open the recorded checkout and STAND in it (callers below resolve relative
/// paths against the cwd). Fails loudly when there is no entry or it is stale.
pub fn open(session: &str) -> Result<GitTransport, String> {
    open_in(&base()?, session)
}

fn open_in(base: &Path, session: &str) -> Result<GitTransport, String> {
    let path = entry_in(base, session);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no workspace is recorded for session {session:?} ({}: {e}). A session's first \
             prompt records its checkout; this is a session whose record is gone -- most \
             likely resumed in a fresh container, where ~/.cache is empty. Nothing was done.",
            path.display()
        )
    })?;
    let entry: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is unreadable: {e}", path.display()))?;
    let checkout = entry
        .get("checkout")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{} names no checkout", path.display()))?;
    let t = GitTransport::discover(checkout).map_err(|e| {
        format!("the checkout recorded for session {session:?}, {checkout}, cannot be opened: {e}")
    })?;
    std::env::set_current_dir(t.work_dir())
        .map_err(|e| format!("entering {}: {e}", t.work_dir().display()))?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_entry_is_an_error_naming_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let error = open_in(dir.path(), "s1").err().unwrap();
        assert!(
            error.contains("no workspace is recorded for session \"s1\""),
            "{error}"
        );
    }

    #[test]
    fn a_deleted_checkout_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        record_in(dir.path(), "s1", Path::new("/nonexistent/checkout")).unwrap();
        let error = open_in(dir.path(), "s1").err().unwrap();
        assert!(error.contains("/nonexistent/checkout"), "{error}");
    }

    #[test]
    fn session_ids_cannot_escape_the_directory() {
        let path = entry_in(Path::new("/b"), "../../etc/x");
        assert_eq!(path, Path::new("/b/______etc_x.json"));
    }
}
