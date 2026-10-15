//! Where a new MCP session's server and conversation-base commit come from
//! when neither is the local checkout the hook process happens to stand in.
//!
//! `workspace.rs` records, per session, whatever [`caos::GitTransport`] its
//! first prompt resolved -- this module is what that resolution now IS,
//! instead of unconditionally trusting the hook's own cwd/`$CLAUDE_PROJECT_DIR`
//! discovery. Two independent things a session can name, each on its own
//! leading prompt line (same convention `resume::parse_command` uses for
//! `/fork-caos-conversation`/`/resume-caos-conversation`: a line counts only
//! as a leading one, so a prompt that merely quotes a directive further down
//! is not a request):
//!
//!   /caos-session-server <url>              -- which caos server
//!   /caos-session-start-from <git locator>  -- which commit seeds the content
//!
//! Neither needs to be given. A server falls back to `CAOS_MCP_DEFAULT_SERVER`,
//! then to the hook's own discovered checkout (unchanged pre-existing
//! behaviour, for anyone still running the MCP server from inside a real
//! local repo). A start-from locator falls back to
//! `CAOS_MCP_DEFAULT_SESSION_BASE`, then to this module's own default,
//! `metta-ai/caos-session`'s `main`.
//!
//! A NAMED server always creates a fresh per-session SCRATCH checkout rather
//! than reusing whatever the hook's cwd discovered: naming one is a request
//! for a session that has nothing to do with wherever this process happens to
//! be standing, and silently reusing a lying-around local checkout instead
//! would mix that session's history with an unrelated directory -- the exact
//! failure this exists to avoid. The scratch checkout needs no human-meaningful
//! content; it exists only to give git something to push conversation objects
//! through and a `caos` remote to carry the server address, exactly as a real
//! checkout's `caos` remote does today (`GitTransport::server_url`, unchanged).
//!
//! `default_server_scratch` is the SAME idea one layer further out: a single,
//! server-WIDE (not session-keyed) scratch checkout for callers that run
//! before any session is known at all -- `caos-cli.rs`'s own `cc_transport()`,
//! consulted for `tools/list` (resolved once per connection, so it has no
//! session to key off of even in principle), `caos mcp warm` and `caos mcp
//! diag`. Without it those all stay hard-failed in a scratch-only deployment
//! even once a session's own first prompt has a perfectly good per-session
//! checkout recorded -- `tools/list`'s cached-once reply would stay the
//! degraded stand-in for the MCP connection's entire lifetime, no matter how
//! many sessions later connect successfully through it.
//!
//! The content base itself is fetched directly with `git fetch`, NOT
//! resolved through `caos::eval_remote_arg` (`:@@=`'s own machinery, measured
//! against the real server and reverted -- see `fetch_commit`'s doc comment).
//! A locator here still uses the SAME string syntax `:@@=` does
//! (`git+`/`github:` scheme, `?ref=`/`rev=`), just parsed by this module's
//! own small reader rather than `parse_git_ref`, which refuses anything
//! carrying a mutable `ref=` by design (`git-locator`'s whole point).

use std::path::PathBuf;

use caos::GitTransport;

use crate::host_git;

const SERVER_COMMAND: &str = "caos-session-server";
const START_FROM_COMMAND: &str = "caos-session-start-from";
const SERVER_ENV: &str = "CAOS_MCP_DEFAULT_SERVER";
const BASE_ENV: &str = "CAOS_MCP_DEFAULT_SESSION_BASE";
const DEFAULT_SESSION_BASE: &str = "github:Metta-AI/caos-session?ref=refs/heads/main";

/// Both directives, read from the prompt's leading lines. Each independently
/// optional; scanning stops at the first line that is neither, so a directive
/// below ordinary prompt text is just text, not a request -- matching
/// `resume::parse_command`'s "first line only" rule, extended to allow two
/// consecutive leading lines since there are now two independent directives.
fn leading_directives(prompt: &str) -> (Option<String>, Option<String>) {
    let (mut server, mut start_from) = (None, None);
    for line in prompt.trim_start().lines() {
        let Some(rest) = line.trim().strip_prefix('/') else {
            break;
        };
        let mut words = rest.split_whitespace();
        match words.next() {
            Some(SERVER_COMMAND) => server = words.next().map(str::to_string),
            Some(START_FROM_COMMAND) => start_from = words.next().map(str::to_string),
            _ => break,
        }
    }
    (server, start_from)
}

/// The transport a session's FIRST prompt should record
/// (`workspace::record`). `fallback` is whatever the hook's own cwd /
/// `$CLAUDE_PROJECT_DIR` discovery already found (today's entire mechanism,
/// unchanged) -- used only when NEITHER a `/caos-session-server` directive
/// nor `CAOS_MCP_DEFAULT_SERVER` names a server.
pub(super) fn open_for_new_session(
    session: &str,
    prompt: &str,
    fallback: Result<GitTransport, String>,
) -> Result<GitTransport, String> {
    let (server_directive, _) = leading_directives(prompt);
    match server_directive.or_else(|| std::env::var(SERVER_ENV).ok()) {
        Some(url) => ensure_scratch_checkout(session, &url),
        None => fallback,
    }
}

/// A server-wide (not session-keyed) scratch checkout, for a caller with no
/// session identity to key off of at all -- see the module doc comment.
/// `None` when `CAOS_MCP_DEFAULT_SERVER` is not set, so a caller's EXISTING
/// error (a real repository genuinely not found) passes through unchanged
/// rather than being replaced by a less informative one.
pub fn default_server_scratch() -> Option<Result<GitTransport, String>> {
    let url = std::env::var(SERVER_ENV).ok()?;
    Some(ensure_scratch_checkout("_server", &url))
}

/// Where a session's scratch checkout lives -- out of the way, not meant to
/// be found by a person, same convention `workspace.rs`'s own
/// `~/.cache/caos/workspaces` uses for its session records.
fn scratch_dir(session: &str) -> Result<PathBuf, String> {
    let home =
        std::env::var_os("HOME").ok_or("HOME is not set, so no scratch checkout can be created")?;
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
    Ok(PathBuf::from(home)
        .join(".cache/caos/mcp-scratch")
        .join(name))
}

/// Create (if needed) and open a session's scratch checkout, with its `caos`
/// remote pointed at `server_url`. Idempotent: a session's first prompt is
/// the only caller, but re-affirming the remote rather than erroring if one
/// is already there costs nothing and survives a crashed first attempt.
fn ensure_scratch_checkout(session: &str, server_url: &str) -> Result<GitTransport, String> {
    let dir = scratch_dir(session)?;
    if !dir.join(".git").is_dir() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        host_git::capture_required("git", &["init", "--quiet"], &dir)?;
    }
    // "remote remove" errors when absent (first time); ignored on purpose --
    // what matters is the "add" right after it, which must always succeed.
    let _ = host_git::capture_required("git", &["remote", "remove", "caos"], &dir);
    host_git::capture_required("git", &["remote", "add", "caos", server_url], &dir)?;
    GitTransport::discover(&dir)
}

/// The conversation's content base when no `--base=<sha>` was given on the
/// command line (the CLI flag, unchanged, still wins over everything here --
/// see `root_commit`). `t` must already be the session's resolved transport
/// (`open_for_new_session`'s result), so the fetch below lands in the right
/// checkout and `ensure_pushed` (inside `root_commit`'s existing
/// `ensure_code_commit`/`seed_content`, both UNCHANGED) ships it to the right
/// server right after.
pub(super) fn resolve_default_base(t: &GitTransport, prompt: &str) -> Result<String, String> {
    let (_, start_from) = leading_directives(prompt);
    let locator = start_from
        .or_else(|| std::env::var(BASE_ENV).ok())
        .unwrap_or_else(|| DEFAULT_SESSION_BASE.to_string());
    fetch_commit(t, &locator)
}

/// Fetch `locator`'s commit directly into `t`'s checkout and return its sha.
///
/// Deliberately NOT `caos::eval_remote_arg` (`:@@=`'s own resolution,
/// `/eval?root:@@=<locator>` on the server) -- tried that first, and a real
/// session against the dev stack came back "object ... is not a commit".
/// `:@@=` evaluates a `.caos-expr` chain, which is exactly right for an IMAGE
/// locator (`--llm-step:@@=...`) and exactly wrong here: `caos-session`'s own
/// root has one (it IS a caos entry), so `/eval` returned what THAT evaluates
/// to, not the raw commit. A conversation's content base wants the commit's
/// raw tree, unevaluated -- the same thing local `HEAD` always gave it. A
/// plain `git fetch` gets exactly that, and lands the object genuinely
/// locally, so the existing `ensure_code_commit`/`seed_content` pipeline
/// (`root_commit`, untouched) can push it to the server exactly as it already
/// pushes any local commit -- no server-side evaluation involved at all.
///
/// A `dir=` component is not supported here (only `rev=`/`ref=` are read): a
/// conversation base is a whole commit's tree, not a subdirectory of one --
/// unlike an image locator, which `:@@=` can descend into after resolving.
fn fetch_commit(t: &GitTransport, locator: &str) -> Result<String, String> {
    let (url, _) = locator.split_once('?').unwrap_or((locator, ""));
    let fetch_url = fetch_url(url);
    let want = query_value(locator, "rev")
        .or_else(|| query_value(locator, "ref"))
        .ok_or_else(|| format!("session base locator {locator:?} names no ref= or rev="))?;
    host_git::capture_required(
        "git",
        &["fetch", "--quiet", &fetch_url, &want],
        t.work_dir(),
    )
    .map_err(|e| format!("fetching {want} from {fetch_url}: {e}"))?;
    host_git::capture_required("git", &["rev-parse", "FETCH_HEAD"], t.work_dir())
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("resolving FETCH_HEAD after fetching {want} from {fetch_url}: {e}"))
}

/// The URL `git fetch` itself understands, from a locator's `git+`/`github:`
/// scheme -- the same unwrapping `GitRef::fetch_url` does, duplicated here
/// because building a real `GitRef` requires `parse_git_ref`, which refuses
/// to parse anything carrying `ref=` in the first place.
fn fetch_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("git+") {
        return rest.to_string();
    }
    if let Some(rest) = url.strip_prefix("github:") {
        return format!("https://github.com/{rest}");
    }
    url.to_string()
}

/// Pull one `key=value` out of a `:@@=` locator's query string. Only ever
/// called on `ref`, whose value is a plain branch/tag name -- no percent-
/// decoding, matching how `parse_git_ref` itself reads the query.
fn query_value(locator: &str, key: &str) -> Option<String> {
    let (_, query) = locator.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_directives_reads_both_in_either_order() {
        let (server, start_from) = leading_directives(
            "/caos-session-server http://localhost:9090\n\
             /caos-session-start-from github:Metta-AI/caos-session?ref=refs/heads/main\n\
             do the thing",
        );
        assert_eq!(server, Some("http://localhost:9090".to_string()));
        assert_eq!(
            start_from,
            Some("github:Metta-AI/caos-session?ref=refs/heads/main".to_string())
        );
    }

    #[test]
    fn leading_directives_absent_is_fine() {
        assert_eq!(leading_directives("just a normal prompt"), (None, None));
    }

    #[test]
    fn a_directive_below_prompt_text_is_not_a_request() {
        let (server, _) = leading_directives("do the thing\n/caos-session-server http://x");
        assert_eq!(server, None);
    }

    #[test]
    fn query_value_finds_ref_among_other_keys() {
        assert_eq!(
            query_value("github:o/r?dir=std/x&ref=refs/heads/main", "ref"),
            Some("refs/heads/main".to_string())
        );
        assert_eq!(query_value("github:o/r?rev=abc", "ref"), None);
    }

    #[test]
    fn fetch_url_unwraps_both_schemes() {
        assert_eq!(
            fetch_url("github:Metta-AI/caos"),
            "https://github.com/Metta-AI/caos"
        );
        assert_eq!(
            fetch_url("git+https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn a_locator_with_neither_rev_nor_ref_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet"], dir.path()).unwrap();
        let t = GitTransport::discover(dir.path()).unwrap();
        let error = fetch_commit(&t, "github:Metta-AI/caos-session").unwrap_err();
        assert!(error.contains("names no ref= or rev="), "{error}");
    }
}
