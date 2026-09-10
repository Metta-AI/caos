//! Shared parser for pinned Git locators and local directory locators.
/// A parsed `:@@=` locator — a git tree named by WHERE to fetch it, pinned by a
/// content hash (design/flake-inputs.md). The syntax is nix's flake-reference
/// grammar, borrowed as a STRING FORMAT only (no nix ever runs): a scheme + an
/// optional `?rev=<sha>&dir=<subpath>` query.
///
/// The pin is what makes a URL — a *name* — behave like content: `rev` (a
/// full-length commit sha) is MANDATORY for a git fetch, and the client resolves
/// `url@rev → oid` at eval time so the oid, never the URL, enters the arg tree /
/// cache key. A `path:` names a plain local directory instead (hashed live, like
/// `:@=`), so it carries no rev. the caller is the resolution these
/// fields drive.
#[derive(Debug, PartialEq, Eq)]
pub struct GitRef {
    /// The fetch URL — everything before `?`: `git+https://…`, `git+ssh://…`,
    /// `git+file://…`, `github:owner/repo`, or `path:<dir>`.
    pub url: String,
    /// The pinned commit sha: `Some` (and 40-hex) for a git fetch, `None` for a
    /// `path:` plain directory.
    pub rev: Option<String>,
    /// The subtree within the fetched repo to descend into (`dir=`), if given.
    pub dir: Option<String>,
}

impl GitRef {
    /// A `path:` locator names a plain local directory — no git fetch, no rev —
    /// resolved by ingesting the tree, like a `:@=` path.
    pub fn is_plain_dir(&self) -> bool {
        self.url.starts_with("path:")
    }

    /// The URL to hand `git fetch`. The locator's scheme is nix's, which prefixes
    /// a transport with `git+` and abbreviates GitHub — neither of which git
    /// itself understands, so the `git+` comes off and `github:o/r` expands to
    /// the HTTPS URL it stands for. This is the ONE place the sugar is undone:
    /// the parsed `url` stays exactly what the caller wrote, so an error message
    /// quotes their locator rather than something normalized behind their back.
    pub fn fetch_url(&self) -> String {
        if let Some(rest) = self.url.strip_prefix("git+") {
            return rest.to_string();
        }
        if let Some(rest) = self.url.strip_prefix("github:") {
            return format!("https://github.com/{rest}");
        }
        self.url.clone()
    }
}

/// Parse a `:@@=` locator value into a [`GitRef`], validating the
/// content-addressing invariant: a git fetch MUST pin a commit (`rev=<40-hex>`),
/// a mutable `ref=` (branch/tag) is rejected, and a `path:` takes no rev. The
/// scheme chooses the meaning — never sniffed from the value's shape. Pure
/// string logic (unit-tested); the caller does the fetching.
pub fn parse_git_ref(value: &str) -> Result<GitRef, String> {
    let (url, query) = value.split_once('?').unwrap_or((value, ""));
    if url.is_empty() {
        return Err(format!("git ref {value:?} has no scheme/url"));
    }
    // A git fetch (rev mandatory) vs a plain local directory (no rev). The
    // scheme decides — chosen by the operator, not guessed.
    let is_git_fetch = url.starts_with("git+") || url.starts_with("github:");
    let is_plain_dir = url.starts_with("path:");
    if !is_git_fetch && !is_plain_dir {
        return Err(format!(
            "git ref {value:?}: unknown scheme; use git+https://…, git+ssh://…, \
             git+file://…, github:owner/repo, or path:<dir>"
        ));
    }

    let (mut rev, mut dir, mut has_ref) = (None, None, false);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("git ref {value:?}: query part {pair:?} is not key=value"))?;
        match k {
            "rev" if rev.replace(v.to_string()).is_some() => {
                return Err(format!("git ref {value:?}: rev given twice"))
            }
            "dir" if dir.replace(v.to_string()).is_some() => {
                return Err(format!("git ref {value:?}: dir given twice"))
            }
            "rev" | "dir" => {}
            "ref" => has_ref = true,
            other => {
                return Err(format!(
                    "git ref {value:?}: unknown query key {other:?} (use rev=, dir=)"
                ))
            }
        }
    }

    if is_git_fetch {
        // A mutable ref (branch/tag) is not content; and a git fetch with no pin
        // at all is not content-addressed. Both are rejected — that rejection is
        // the whole point of making rev mandatory.
        if has_ref {
            return Err(format!(
                "git ref {value:?}: a `ref=` (branch/tag) is mutable; pin a commit with `rev=`"
            ));
        }
        match &rev {
            None => {
                return Err(format!(
                    "git ref {value:?}: a remote ref must pin a commit — add `rev=<40-hex sha>`"
                ))
            }
            Some(r) if !(r.len() == 40 && r.bytes().all(|byte| byte.is_ascii_hexdigit())) => {
                return Err(format!(
                    "git ref {value:?}: rev must be a full-length commit sha, got {r:?}"
                ))
            }
            Some(_) => {}
        }
    } else if rev.is_some() {
        // path: is a live local directory, so a rev is meaningless.
        return Err(format!(
            "git ref {value:?}: a `path:` names a local directory and takes no `rev=`"
        ));
    }

    Ok(GitRef {
        url: url.to_string(),
        rev,
        dir,
    })
}

#[cfg(test)]
mod git_ref_tests {
    use super::{parse_git_ref, GitRef};

    fn r(url: &str, rev: Option<&str>, dir: Option<&str>) -> GitRef {
        GitRef {
            url: url.to_string(),
            rev: rev.map(String::from),
            dir: dir.map(String::from),
        }
    }
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567"; // 40 hex

    #[test]
    fn git_https_with_rev_and_dir() {
        let v = format!("git+https://github.com/o/repo?rev={SHA}&dir=std/deep-deps");
        assert_eq!(
            parse_git_ref(&v).unwrap(),
            r(
                "git+https://github.com/o/repo",
                Some(SHA),
                Some("std/deep-deps")
            )
        );
    }

    #[test]
    fn git_ssh_and_file_and_github_take_a_rev() {
        for url in [
            "git+ssh://git@github.com/o/repo",
            "git+file:///abs/repo",
            "github:o/repo",
        ] {
            let v = format!("{url}?rev={SHA}");
            assert_eq!(parse_git_ref(&v).unwrap(), r(url, Some(SHA), None));
        }
    }

    #[test]
    fn path_is_a_plain_dir_with_no_rev() {
        let g = parse_git_ref("path:./some/dir").unwrap();
        assert_eq!(g, r("path:./some/dir", None, None));
        assert!(g.is_plain_dir());
        // dir= is still allowed on a path: (a subtree of the local dir).
        assert_eq!(
            parse_git_ref("path:./x?dir=sub").unwrap(),
            r("path:./x", None, Some("sub"))
        );
    }

    #[test]
    fn a_git_fetch_must_pin_a_commit() {
        // no rev at all
        assert!(parse_git_ref("git+https://h/r")
            .unwrap_err()
            .contains("must pin a commit"));
        assert!(parse_git_ref("git+https://h/r?dir=x")
            .unwrap_err()
            .contains("must pin a commit"));
    }

    #[test]
    fn a_mutable_ref_is_rejected() {
        let e = parse_git_ref("git+https://h/r?ref=main").unwrap_err();
        assert!(e.contains("mutable"), "{e}");
        // even alongside a rev, a ref= is refused (ambiguous, and invites drift).
        let v = format!("git+https://h/r?ref=main&rev={SHA}");
        assert!(parse_git_ref(&v).unwrap_err().contains("mutable"));
    }

    #[test]
    fn rev_must_be_a_full_sha() {
        assert!(parse_git_ref("git+https://h/r?rev=abc123")
            .unwrap_err()
            .contains("full-length commit sha"));
    }

    #[test]
    fn path_rejects_a_rev() {
        let v = format!("path:./x?rev={SHA}");
        assert!(parse_git_ref(&v).unwrap_err().contains("takes no `rev=`"));
    }

    #[test]
    fn unknown_scheme_or_query_key() {
        assert!(parse_git_ref("https://h/r?rev=x")
            .unwrap_err()
            .contains("unknown scheme"));
        let v = format!("git+https://h/r?rev={SHA}&frob=1");
        assert!(parse_git_ref(&v).unwrap_err().contains("unknown query key"));
    }

    #[test]
    fn malformed_query_and_dupes() {
        let v = format!("git+https://h/r?rev={SHA}&dir");
        assert!(parse_git_ref(&v).unwrap_err().contains("not key=value"));
        let v = format!("git+https://h/r?rev={SHA}&rev={SHA}");
        assert!(parse_git_ref(&v).unwrap_err().contains("rev given twice"));
    }
}
