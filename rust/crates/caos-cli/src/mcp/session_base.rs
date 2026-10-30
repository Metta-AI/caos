//! Where a new session's conversation content comes from: a git locator naming a
//! COMMIT.
//!
//! The harness puts whatever it wants the conversation to start from into a git
//! repository (committing and pushing dirty files first, if it has them) and names
//! the commit by locator. The hook does not read a checkout, push anything or
//! resolve the locator: it hands the step `content-locator=<locator>` with the
//! first prompt, and the step imports the commit, with its history, and records
//! it -- as the parent of a new conversation's root, or merged into a fork
//! (`std/llm-step/src/record.rs`). The commit rather than its tree, because that
//! history is what lets git find the base when the repository's newer commit is
//! merged in later.
//!
//! Two places can name the locator, most specific first:
//!
//! ```text
//! /caos-session-start-from <git locator>   a leading line of the first prompt
//! --base:@@=<git locator>                  the hook's and the tool server's argument
//! ```
//!
//! A directive counts only as a LEADING line, so a prompt that merely quotes one
//! further down is not a request (the rule `fork::parse_command` uses for
//! `/fork-caos-conversation`). With none of them the conversation starts without
//! code.
//!
//! A locator pins its commit (`rev=<sha>`). It is passed as written: the step
//! parses it (`git_locator::parse_git_ref`) and fetches the commit it names.

const START_FROM_COMMAND: &str = "caos-session-start-from";

/// The locator on the prompt's leading `/caos-session-start-from` line, if any.
fn leading_start_from(prompt: &str) -> Option<String> {
    let first = prompt.trim_start().lines().next()?;
    let mut words = first.trim().strip_prefix('/')?.split_whitespace();
    if words.next() != Some(START_FROM_COMMAND) {
        return None;
    }
    words.next().map(str::to_string)
}

/// The content locator for a new conversation, ready to send as `content-locator`.
pub(super) fn content_locator(configured: Option<&str>, prompt: &str) -> Option<String> {
    leading_start_from(prompt).or_else(|| configured.map(str::to_string))
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
    fn the_prompt_beats_the_argument_and_the_locator_is_passed_as_written() {
        let prompt = format!("/caos-session-start-from {PINNED}\n");
        assert_eq!(
            content_locator(Some("github:o/other?rev=abc"), &prompt),
            Some(PINNED.to_string())
        );
        assert_eq!(
            content_locator(Some("github:o/other?rev=abc"), "hello"),
            Some("github:o/other?rev=abc".to_string())
        );
        assert_eq!(content_locator(None, "hello"), None);
    }
}
