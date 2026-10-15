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

use std::path::{Path, PathBuf};

use caos::{GitTransport, Transport};

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
    let fetched = fetch_commit(t, &locator)?;
    repoint_to_dev(t, &fetched)
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

/// The current tip of `server`'s own `refs/caos/dev`, if it has one -- the
/// SAME probe `scripts/caos-mcp-launch` and the Hermes `caos-hooks` plugin
/// use for `--llm-step`, kept in sync by hand across the three. `caosd up`
/// publishes the live, uncommitted working tree there and seeds the core
/// against that exact same tree (AGENTS.md's seeded-core notes), so it is
/// the one locator that always matches what a dev/test stack actually
/// answers for.
///
/// `None`, never an error, for an unreachable server or one with no such
/// ref -- a real production deploy with nothing published there, the common
/// case `repoint_to_dev` must fall through on rather than break. Bounded
/// (`timeout`): a dead address does not refuse a connection, it hangs out a
/// TCP retransmit (AGENTS.md, "Caches and defaults").
fn probe_dev_sha(server: &str) -> Option<String> {
    let output = std::process::Command::new("timeout")
        .args(["10", "git", "ls-remote", server, "refs/caos/dev"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Repoint every `:@@=<scheme>?rev=<hex>&dir=<path>` locator found in a
/// `.caos-expr`'s text at `server`/`dev_sha`, keeping each locator's own
/// `dir=` unchanged. Mirrors `integrations/claude-code/cloud/bootstrap.go`'s
/// `seedCommit`, whose Go regex is
/// `:@@=[^ ?]*\?rev=[0-9a-f]*&dir=([^ ]*)` -- reimplemented here as plain
/// string scanning rather than pulling in a `regex` dependency this crate
/// has none of (anchoring one into std/cargo's offline vendored registry for
/// one callsite is not worth it; AGENTS.md, "Before committing").
///
/// `None`, not an error, when the text carries no such locator at all --
/// NOTHING TO REWRITE is a normal outcome (a session-base repo that doesn't
/// mount caos this way), distinct from a malformed one.
fn rewrite_locators(expr: &str, server: &str, dev_sha: &str) -> Option<String> {
    const NEEDLE: &str = ":@@=";
    let mut out = String::with_capacity(expr.len());
    let mut rest = expr;
    let mut rewrote = false;
    while let Some(at) = rest.find(NEEDLE) {
        let locator_start = at + NEEDLE.len();
        out.push_str(&rest[..locator_start]);
        // `.caos-expr` is a space-separated argument list (design/caos-expr.md),
        // so a locator token never contains whitespace -- it ends at the next
        // one, or at end of text for a locator on the file's last line.
        let token_len = rest[locator_start..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - locator_start);
        let token = &rest[locator_start..locator_start + token_len];
        match token
            .split_once("?rev=")
            .and_then(|(_, q)| q.split_once("&dir="))
        {
            // Shaped like the thing we rewrite: swap scheme+rev, keep dir.
            Some((_, dir)) => {
                out.push_str(&format!("git+{server}?rev={dev_sha}&dir={dir}"));
                rewrote = true;
            }
            // A `path:`/`hash:` locator, or a `?rev=`/`&dir=` ordered some
            // other way -- left exactly as committed, same as the Go regex
            // simply not matching it.
            None => out.push_str(token),
        }
        rest = &rest[locator_start + token_len..];
    }
    out.push_str(rest);
    rewrote.then_some(out)
}

/// Repoint `flake.lock`'s `"caos"` input node at `server`/`dev_sha`,
/// mirroring `bootstrap.go`'s equivalent: `{type: git, url: server, rev:
/// dev_sha}`, replacing the `github`-locked original (`narHash`/
/// `lastModified` don't apply to a `git+` fetch pinned by an explicit rev,
/// same as `bootstrap.go` drops them).
///
/// `Ok(None)` -- not an error -- when the lock has no `"caos"` input at all:
/// nothing to rewrite, e.g. a session-base repo that doesn't declare one.
/// Only a genuinely MALFORMED flake.lock (unparseable JSON) is an error: a
/// tree carrying a `.caos-expr` locator worth rewriting but a lock the
/// flake-input-loader can't even read would otherwise seed a session the
/// loader refuses outright anyway, which is a worse failure to discover
/// than refusing to seed it here first.
fn rewrite_flake_lock(
    lock_text: &str,
    server: &str,
    dev_sha: &str,
) -> Result<Option<String>, String> {
    let mut lock: serde_json::Value =
        serde_json::from_str(lock_text).map_err(|error| format!("parsing as JSON: {error}"))?;
    let root_name = lock
        .get("root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("root")
        .to_string();
    let caos_key = lock
        .get("nodes")
        .and_then(|nodes| nodes.get(&root_name))
        .and_then(|root| root.get("inputs"))
        .and_then(|inputs| inputs.get("caos"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let Some(caos_key) = caos_key else {
        return Ok(None);
    };
    let Some(node) = lock
        .get_mut("nodes")
        .and_then(|nodes| nodes.get_mut(&caos_key))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(None);
    };
    node.insert(
        "locked".to_string(),
        serde_json::json!({ "type": "git", "url": server, "rev": dev_sha }),
    );
    let text =
        serde_json::to_string_pretty(&lock).map_err(|error| format!("re-encoding: {error}"))?;
    Ok(Some(format!("{text}\n")))
}

/// Stage `content` as a blob at `name` in the index named by `env`'s
/// `GIT_INDEX_FILE` -- `hash-object` needs a real path (no stdin-capable
/// helper in `host_git`, and piping stdin through `Command` for one caller
/// is not worth adding), so this writes a temp file first.
fn stage_blob(dir: &Path, env: &[(&str, &str)], name: &str, content: &str) -> Result<(), String> {
    let tmp = tempfile::NamedTempFile::new()
        .map_err(|error| format!("could not stage {name}: {error}"))?;
    std::fs::write(tmp.path(), content)
        .map_err(|error| format!("could not write {name}'s rewritten content: {error}"))?;
    let path = tmp.path().to_string_lossy().to_string();
    let blob = host_git::capture_required_env("git", &["hash-object", "-w", &path], dir, env)?;
    host_git::capture_required_env(
        "git",
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{name}"),
        ],
        dir,
        env,
    )?;
    Ok(())
}

/// Automatic analogue of `integrations/claude-code/cloud/bootstrap.go`'s
/// `seedCommit`, run on EVERY default-based session rather than behind a
/// `--dev-commit` flag: when the server this transport talks to
/// (`t.server_url()`) is publishing its own `refs/caos/dev` (a dev/test
/// stack -- `probe_dev_sha`), repoint `commit`'s root `.caos-expr` (every
/// `:@@=...&dir=...` locator in it) and `flake.lock`'s `"caos"` input node
/// at THAT commit instead of whatever the session-base repo itself has
/// committed.
///
/// THIS IS THE FIX for a session whose default content tree pins caos at
/// one (possibly stale, possibly uncommitted-relative-to) commit while the
/// dev stack it is actually talking to answers `--llm-step` and every other
/// seeded-core lookup for a DIFFERENT one: every `caos-std`-mounted tool in
/// that tree silently ran the wrong caos, or failed outright on a seeded-
/// sentinel mismatch identical in shape to the one `--llm-step` itself used
/// to hit (see `caos-hooks/__init__.py`'s `_dev_sha` and
/// `scripts/caos-mcp-launch`'s matching comment for that half of the fix).
///
/// Returns `commit` UNCHANGED, never erroring on its account, when: the
/// server has no `refs/caos/dev` (a real, git-less production deploy --
/// the common case this must not break); the dev commit already matches;
/// the tree carries no `.caos-expr` at `commit` at all; or it carries one
/// with no locator worth rewriting. A malformed `flake.lock` found ALONGSIDE
/// a locator that DOES need rewriting is the one case that errors -- see
/// `rewrite_flake_lock`'s own doc comment for why.
fn repoint_to_dev(t: &GitTransport, commit: &str) -> Result<String, String> {
    let server = t.server_url()?;
    let Some(dev_sha) = probe_dev_sha(&server) else {
        return Ok(commit.to_string());
    };
    if dev_sha == commit {
        return Ok(commit.to_string());
    }
    let dir = t.work_dir();
    let expr =
        match host_git::capture_required("git", &["show", &format!("{commit}:.caos-expr")], dir) {
            Ok(text) => text,
            // No `.caos-expr` at this commit at all -- nothing to rewrite.
            Err(_) => return Ok(commit.to_string()),
        };
    let Some(rewritten_expr) = rewrite_locators(&expr, &server, &dev_sha) else {
        return Ok(commit.to_string());
    };

    let index = tempfile::NamedTempFile::new()
        .map_err(|error| format!("could not make a seed index: {error}"))?;
    let index_path = index.path().to_string_lossy().to_string();
    let env: [(&str, &str); 5] = [
        ("GIT_INDEX_FILE", &index_path),
        // An identity of its own, because `commit-tree` refuses without one
        // and the ambient git config may have none (same reasoning as
        // `bootstrap.go`'s `seedCommit`). Whose commit this is carries no
        // meaning: nothing ever pushes it, and its only reader is the
        // conversation this hook is seeding.
        ("GIT_AUTHOR_NAME", "caos dev mode"),
        ("GIT_AUTHOR_EMAIL", "caos@localhost"),
        ("GIT_COMMITTER_NAME", "caos dev mode"),
        ("GIT_COMMITTER_EMAIL", "caos@localhost"),
    ];
    host_git::capture_required_env("git", &["read-tree", commit], dir, &env)?;
    stage_blob(dir, &env, ".caos-expr", &rewritten_expr)?;

    if let Ok(lock_text) =
        host_git::capture_required("git", &["show", &format!("{commit}:flake.lock")], dir)
    {
        match rewrite_flake_lock(&lock_text, &server, &dev_sha) {
            Ok(Some(new_lock)) => stage_blob(dir, &env, "flake.lock", &new_lock)?,
            // No "caos" input node -- leave flake.lock exactly as committed.
            Ok(None) => {}
            Err(error) => return Err(format!("rewriting flake.lock for the dev stack: {error}")),
        }
    }

    let tree = host_git::capture_required_env("git", &["write-tree"], dir, &env)?;
    host_git::capture_required_env(
        "git",
        &[
            "commit-tree",
            &tree,
            "-p",
            commit,
            "-m",
            "caos dev mode: repointed at the dev stack",
        ],
        dir,
        &env,
    )
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

    #[test]
    fn rewrite_locators_swaps_scheme_and_rev_keeping_dir() {
        let expr = "run --base:@@=github:Metta-AI/caos?rev=oldsha&dir=std/llm-step --in:@=.";
        let rewritten =
            rewrite_locators(expr, "http://127.0.0.1", "newsha").expect("a locator to rewrite");
        assert_eq!(
            rewritten,
            "run --base:@@=git+http://127.0.0.1?rev=newsha&dir=std/llm-step --in:@=."
        );
    }

    #[test]
    fn rewrite_locators_rewrites_every_occurrence() {
        // caos-session's own `.caos-expr` carries exactly this shape: the
        // locator twice, once for `--base` and once for `--input-tree`.
        let expr = "run --base:@@=github:Metta-AI/caos?rev=abc&dir=std/flake-input-loader \
                    --input-tree:@@=github:Metta-AI/caos?rev=abc&dir=std";
        let rewritten =
            rewrite_locators(expr, "http://127.0.0.1", "def").expect("locators to rewrite");
        assert_eq!(
            rewritten,
            "run --base:@@=git+http://127.0.0.1?rev=def&dir=std/flake-input-loader \
             --input-tree:@@=git+http://127.0.0.1?rev=def&dir=std"
        );
    }

    #[test]
    fn rewrite_locators_leaves_a_non_matching_expr_alone() {
        assert_eq!(
            rewrite_locators("run --base:@=std/hello --in:@=.", "s", "d"),
            None
        );
    }

    #[test]
    fn rewrite_flake_lock_repoints_the_caos_input_node() {
        let lock = serde_json::json!({
            "root": "root",
            "nodes": {
                "root": {"inputs": {"caos": "caos"}},
                "caos": {
                    "locked": {
                        "type": "github", "owner": "Metta-AI", "repo": "caos",
                        "rev": "oldsha", "narHash": "sha256-xxx",
                    },
                    "original": {"type": "github", "owner": "Metta-AI", "repo": "caos", "ref": "main"},
                },
            },
        })
        .to_string();
        let rewritten = rewrite_flake_lock(&lock, "http://127.0.0.1", "newsha")
            .unwrap()
            .expect("a caos input node to rewrite");
        let parsed: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(
            parsed["nodes"]["caos"]["locked"],
            serde_json::json!({"type": "git", "url": "http://127.0.0.1", "rev": "newsha"})
        );
    }

    #[test]
    fn rewrite_flake_lock_is_ok_none_without_a_caos_input() {
        let lock =
            serde_json::json!({"root": "root", "nodes": {"root": {"inputs": {}}}}).to_string();
        assert_eq!(rewrite_flake_lock(&lock, "s", "d").unwrap(), None);
    }

    #[test]
    fn rewrite_flake_lock_errors_on_unparseable_json() {
        let error = rewrite_flake_lock("not json", "s", "d").unwrap_err();
        assert!(error.contains("parsing as JSON"), "{error}");
    }

    #[test]
    fn probe_dev_sha_reads_a_real_refs_caos_dev_and_none_without_one() {
        // A bare repo stands in for "the server": `git ls-remote` works
        // identically over a filesystem path as it does over http, which is
        // what makes this testable without a running caos server at all.
        let bare = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet", "--bare"], bare.path()).unwrap();
        let server = bare.path().to_string_lossy().to_string();
        assert_eq!(probe_dev_sha(&server), None, "a bare repo with no dev ref");

        let work = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet"], work.path()).unwrap();
        host_git::capture_required(
            "git",
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "dev",
                "--no-gpg-sign",
            ],
            work.path(),
        )
        .unwrap();
        let dev_commit =
            host_git::capture_required("git", &["rev-parse", "HEAD"], work.path()).unwrap();
        host_git::capture_required(
            "git",
            &["push", "--quiet", &server, "HEAD:refs/caos/dev"],
            work.path(),
        )
        .unwrap();
        assert_eq!(probe_dev_sha(&server), Some(dev_commit));
    }

    /// End to end: a session-base commit whose `.caos-expr`/`flake.lock` pin
    /// one caos commit gets repointed at a DIFFERENT one the "server" (a
    /// bare repo standing in for it, same trick as the test above) is
    /// publishing on `refs/caos/dev` -- the exact scenario this whole
    /// function exists for.
    #[test]
    fn repoint_to_dev_rewrites_when_the_server_publishes_a_different_commit() {
        let bare = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet", "--bare"], bare.path()).unwrap();
        let server = bare.path().to_string_lossy().to_string();

        let work = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet"], work.path()).unwrap();
        host_git::capture_required("git", &["remote", "add", "caos", &server], work.path())
            .unwrap();
        let expr = "run --base:@@=github:Metta-AI/caos?rev=oldsha&dir=std/llm-step --in:@=.";
        std::fs::write(work.path().join(".caos-expr"), expr).unwrap();
        let lock = serde_json::json!({
            "root": "root",
            "nodes": {
                "root": {"inputs": {"caos": "caos"}},
                "caos": {
                    "locked": {"type": "github", "owner": "Metta-AI", "repo": "caos", "rev": "oldsha"},
                    "original": {"type": "github", "owner": "Metta-AI", "repo": "caos", "ref": "main"},
                },
            },
        })
        .to_string();
        std::fs::write(work.path().join("flake.lock"), &lock).unwrap();
        host_git::capture_required("git", &["add", "-A"], work.path()).unwrap();
        host_git::capture_required(
            "git",
            &["commit", "-q", "-m", "base", "--no-gpg-sign"],
            work.path(),
        )
        .unwrap();
        let commit =
            host_git::capture_required("git", &["rev-parse", "HEAD"], work.path()).unwrap();

        // Publish a DIFFERENT commit as the server's refs/caos/dev.
        host_git::capture_required(
            "git",
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "dev",
                "--no-gpg-sign",
            ],
            work.path(),
        )
        .unwrap();
        let dev_commit =
            host_git::capture_required("git", &["rev-parse", "HEAD"], work.path()).unwrap();
        host_git::capture_required(
            "git",
            &["push", "--quiet", &server, "HEAD:refs/caos/dev"],
            work.path(),
        )
        .unwrap();
        // Back to the original commit -- repoint_to_dev takes an explicit sha.
        host_git::capture_required("git", &["checkout", "--quiet", &commit], work.path()).unwrap();

        let t = GitTransport::discover(work.path()).unwrap();
        let repointed = repoint_to_dev(&t, &commit).unwrap();
        assert_ne!(repointed, commit, "a new commit must have been minted");

        let new_expr = host_git::capture_required(
            "git",
            &["show", &format!("{repointed}:.caos-expr")],
            work.path(),
        )
        .unwrap();
        assert!(
            new_expr.contains(&format!("git+{server}?rev={dev_commit}&dir=std/llm-step")),
            "{new_expr}"
        );
        let new_lock = host_git::capture_required(
            "git",
            &["show", &format!("{repointed}:flake.lock")],
            work.path(),
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&new_lock).unwrap();
        assert_eq!(
            parsed["nodes"]["caos"]["locked"],
            serde_json::json!({"type": "git", "url": server, "rev": dev_commit})
        );
        // The checkout itself (the throwaway index aside) is untouched.
        assert!(host_git::capture_required(
            "git",
            &["show", &format!("{commit}:.caos-expr")],
            work.path()
        )
        .unwrap()
        .contains("rev=oldsha"));
    }

    #[test]
    fn repoint_to_dev_is_a_noop_without_a_dev_ref() {
        let bare = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet", "--bare"], bare.path()).unwrap();
        let server = bare.path().to_string_lossy().to_string();

        let work = tempfile::tempdir().unwrap();
        host_git::capture_required("git", &["init", "--quiet"], work.path()).unwrap();
        host_git::capture_required("git", &["remote", "add", "caos", &server], work.path())
            .unwrap();
        std::fs::write(
            work.path().join(".caos-expr"),
            "run --base:@@=github:Metta-AI/caos?rev=oldsha&dir=std/llm-step --in:@=.",
        )
        .unwrap();
        host_git::capture_required("git", &["add", "-A"], work.path()).unwrap();
        host_git::capture_required(
            "git",
            &["commit", "-q", "-m", "base", "--no-gpg-sign"],
            work.path(),
        )
        .unwrap();
        let commit =
            host_git::capture_required("git", &["rev-parse", "HEAD"], work.path()).unwrap();

        let t = GitTransport::discover(work.path()).unwrap();
        assert_eq!(repoint_to_dev(&t, &commit).unwrap(), commit);
    }
}
