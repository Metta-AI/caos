//! Where a new session's conversation content comes from: a git locator.
//!
//! The harness puts whatever it wants the conversation to start from into a git
//! repository (committing and pushing dirty files first, if it has them) and names
//! the commit by locator. The hook does not read a checkout, push anything or
//! resolve the locator: it hands the server `content:@@=<locator>` with the first
//! prompt, and the server fetches the commit and records its tree.
//!
//! Three places can name the locator, most specific first:
//!
//! ```text
//! /caos-session-start-from <git locator>   a leading line of the first prompt
//! --base:@@=<git locator>                  the hook's and the tool server's argument
//! CAOS_MCP_DEFAULT_SESSION_BASE            the environment
//! ```
//!
//! A directive counts only as a LEADING line, so a prompt that merely quotes one
//! further down is not a request (the rule `resume::parse_command` uses for
//! `/fork-caos-conversation`). With none of them the conversation starts without
//! code.
//!
//! A locator pins its commit (`rev=<sha>`). The content is the commit's own
//! tree, not what evaluating it produces (a repository that mounts caos has a
//! `.caos-expr` at its root, and content is code to read), so `raw=1` is added
//! when the locator lacks it.

const START_FROM_COMMAND: &str = "caos-session-start-from";
const BASE_ENV: &str = "CAOS_MCP_DEFAULT_SESSION_BASE";

/// The locator on the prompt's leading `/caos-session-start-from` line, if any.
fn leading_start_from(prompt: &str) -> Option<String> {
    let first = prompt.trim_start().lines().next()?;
    let mut words = first.trim().strip_prefix('/')?.split_whitespace();
    if words.next() != Some(START_FROM_COMMAND) {
        return None;
    }
    words.next().map(str::to_string)
}

/// The content locator for a new conversation, ready to send as `content:@@=`.
pub(super) fn content_locator(configured: Option<&str>, prompt: &str) -> Option<String> {
    leading_start_from(prompt)
        .or_else(|| configured.map(str::to_string))
        .or_else(|| std::env::var(BASE_ENV).ok().filter(|v| !v.is_empty()))
        .map(|locator| raw(&locator))
}

/// `locator` naming the tree itself.
fn raw(locator: &str) -> String {
    let (_, query) = locator.split_once('?').unwrap_or((locator, ""));
    if query.split('&').any(|pair| pair == "raw=1") {
        locator.to_string()
    } else if query.is_empty() && !locator.contains('?') {
        format!("{locator}?raw=1")
    } else {
        format!("{locator}&raw=1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = "github:o/r?rev=0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_start_from_directive_is_read_from_the_leading_line_only() {
        assert_eq!(
            leading_start_from(&format!("/caos-session-start-from {PINNED}\nfix it")),
            Some(PINNED.to_string())
        );
        assert_eq!(
            leading_start_from(&format!("fix it\n/caos-session-start-from {PINNED}")),
            None
        );
        assert_eq!(leading_start_from("/caos-session-start-from"), None);
        assert_eq!(leading_start_from("/fork-caos-conversation abc"), None);
    }

    #[test]
    fn a_locator_names_the_tree_unevaluated() {
        assert_eq!(raw(PINNED), format!("{PINNED}&raw=1"));
        assert_eq!(raw(&format!("{PINNED}&raw=1")), format!("{PINNED}&raw=1"));
        assert_eq!(raw("github:o/r"), "github:o/r?raw=1");
    }

    #[test]
    fn the_prompt_beats_the_argument() {
        let prompt = format!("/caos-session-start-from {PINNED}\n");
        assert_eq!(
            content_locator(Some("github:o/other?rev=abc"), &prompt),
            Some(format!("{PINNED}&raw=1"))
        );
        assert_eq!(
            content_locator(Some("github:o/other?rev=abc"), "hello"),
            Some("github:o/other?rev=abc&raw=1".to_string())
        );
    }
}
