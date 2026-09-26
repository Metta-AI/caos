//! The `git` a `:@@=` locator fetch runs as.
//!
//! [`import`](crate::import) is the narrow surface a human-driven
//! `import_source` call gets: HTTPS only, because a model chooses that URL. A
//! locator is different — it is written in a `.caos-expr` by whoever wrote the
//! repository, and the grammar has always admitted `git+ssh://`, `git+file://`
//! and `github:` alongside `git+https://` (design/flake-inputs.md). So this
//! keeps `import`'s hardening — a cleared environment, no ambient credential
//! helper, no config injection, no redirects, a timeout — and widens only the
//! protocol whitelist to the schemes the grammar names.
//!
//! **Credentials are the caller's, scoped to one URL.** An HTTPS locator reuses
//! `import`'s helper verbatim, so a token reaches exactly the host and path it
//! was issued for and nothing else. Every other scheme runs with no credential
//! at all: `ssh://` uses whatever key the server's environment gives it, and
//! `file://` needs none.

use std::process::Command;

/// The protocols a locator may name, and the only ones git is allowed to speak.
/// This list IS the documented grammar (README, design/flake-inputs.md); if one
/// grows the other must, and a scheme reaching a `git fetch` without appearing
/// in both is the bug this comment exists to prevent.
///
/// **An unauthenticated transport is not a weaker guarantee here, and that is
/// why `git://` and `http://` are listed.** What protects the content is the
/// PIN, not the connection: an object's name is the hash of its bytes, the
/// locator names a full commit sha, and `index-pack` recomputes every name it
/// receives — so a hostile server cannot substitute content under a pinned rev
/// without a SHA-1 collision, which git's collision-detecting SHA-1 rejects.
/// (Measured: flipping one byte in a source pack fails the fetch with `packed
/// object … is corrupt`.) `server::locator` then re-verifies by resolving the
/// rev and publishes with `index-pack --strict`. What an unauthenticated
/// transport does cost is confidentiality and availability — an observer sees
/// what is fetched, an active attacker can deny it — and neither can change
/// what enters a cache key.
///
/// `http` is not a test affordance: dev mode writes one. `repointAtDev`
/// (integrations/claude-code/cloud/bootstrap.go) rewrites a pin to
/// `git+<--server>`, and `--server` may be an `http://` URL for a server
/// reachable without a ticket.
///
/// `caos` is deliberately ABSENT: a `git+caos://` locator names the caos server
/// itself, so it is answered from that server's own object store rather than
/// fetched (`server::locator`), and there is no `git-remote-caos` here to speak
/// it with.
const PROTOCOLS: &[&str] = &["https", "http", "git", "ssh", "file"];

/// Build the `git` a locator fetch runs as, for `url` (already a git-speakable
/// URL — `GitRef::fetch_url` has undone the `git+`/`github:` sugar).
pub fn fetch_command(url: &str, token: Option<&str>) -> Result<Command, String> {
    if url.starts_with("https://") {
        return crate::import::git(url, token);
    }
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .ok_or_else(|| format!("locator url {url:?} names no protocol"))?;
    if !PROTOCOLS.contains(&scheme) {
        return Err(format!(
            "locator url {url:?} uses {scheme:?}, which this server will not fetch \
             (allowed: {})",
            PROTOCOLS.join(", ")
        ));
    }
    if url.bytes().any(|c| c <= b' ' || c >= 127) {
        return Err(format!(
            "locator url {url:?} has a control or non-ASCII byte"
        ));
    }
    if token.is_some() {
        return Err(format!(
            "a Git token is only carried over https://, not {scheme}://"
        ));
    }
    let mut cmd = Command::new("timeout");
    cmd.args(["--kill-after=5", "300", "git"]);
    cmd.env_clear();
    for name in ["PATH", "SSL_CERT_FILE", "GIT_SSL_CAINFO", "HOME"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", scheme)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.extraHeader=",
            "-c",
            "fetch.writeCommitGraph=false",
        ]);
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_s_schemes_are_the_fetchable_ones() {
        for url in [
            "https://github.com/o/r",
            "http://10.0.0.2:9418/r",
            "git://10.0.0.2/r",
            "ssh://git@github.com/o/r",
            "file:///srv/r",
        ] {
            assert!(fetch_command(url, None).is_ok(), "{url}");
        }
        // `caos://` is this server; nothing fetches it. `ext::` is a command.
        for url in ["caos://ticket/r", "ext::sh -c id", "/tmp/r", "o/r"] {
            assert!(fetch_command(url, None).is_err(), "{url}");
        }
    }

    /// EVERY fetchable scheme still has to pin a commit — the property the
    /// grammar exists for, checked against the whitelist itself rather than a
    /// list written out by hand, so adding a protocol cannot quietly add one
    /// that takes a branch name.
    #[test]
    fn every_fetchable_scheme_must_pin_a_commit() {
        for scheme in PROTOCOLS {
            let base = format!("git+{scheme}://host/repo");
            assert!(
                crate::parse_git_ref(&base)
                    .unwrap_err()
                    .contains("must pin a commit"),
                "{base} was accepted without a rev"
            );
            assert!(
                crate::parse_git_ref(&format!("{base}?ref=main"))
                    .unwrap_err()
                    .contains("mutable"),
                "{base} accepted a branch name"
            );
            assert!(
                crate::parse_git_ref(&format!("{base}?rev=abc123"))
                    .unwrap_err()
                    .contains("full-length"),
                "{base} accepted a short rev"
            );
            let pinned = format!("{base}?rev={}", "0".repeat(40));
            assert!(crate::parse_git_ref(&pinned).is_ok(), "{pinned}");
        }
    }

    /// The whitelist and the scheme list the PARSER suggests are two statements
    /// of one rule (design/flake-inputs.md). A protocol reachable by `git fetch`
    /// but absent from the error text is a format that grew without being
    /// written down, which is how the two drift.
    #[test]
    fn the_documented_schemes_are_the_fetchable_ones() {
        let message = crate::parse_git_ref("nope://host/r").unwrap_err();
        for scheme in PROTOCOLS {
            assert!(
                message.contains(&format!("git+{scheme}://")),
                "{scheme} is fetchable but the grammar's error text omits it: {message}"
            );
        }
    }

    #[test]
    fn a_token_rides_only_on_https() {
        assert!(fetch_command("https://h/r", Some("t")).is_ok());
        assert!(fetch_command("ssh://h/r", Some("t"))
            .unwrap_err()
            .contains("only carried over https"));
    }
}
