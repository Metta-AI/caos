//! Picking a recorded conversation up in a NEW Claude Code session.
//!
//! One command, named by the conversation COMMIT it starts from (the hash
//! `caos head <hash>` printed after a turn -- a commit is immutable, where a ref
//! moves):
//!
//! ```text
//! /fork-caos-conversation <hash>     a NEW branch: the new session's own
//!                                    `cc/<session>` conversation, whose
//!                                    history is <hash>'s, then diverges
//! ```
//!
//! It is recognised by the `UserPromptSubmit` hook on a session's first prompt,
//! so the conversation exists -- and holds the earlier history and workspace --
//! before the model takes its first turn. What the MODEL sees of that history is
//! the slash command's own text (see `cloud/install.go`); this file is the record
//! side.
//!
//! The fork itself is the step's (`llm-step --record=prompt --from=<hash>`): it
//! settles the source, forks it and records the prompt on the server. A fork
//! needs no bookkeeping here -- its id is derived from the session id like any
//! other, so a session's conversation is always `conversation_id_for(session)`.

use conversation_protocol::v3::oid::Oid;

use crate::oid;

pub(super) const FORK_COMMAND: &str = "fork-caos-conversation";

/// A line the slash command's expanded text carries, so the hook still finds the
/// request if Claude Code hands it the expansion rather than what was typed.
/// `caos-conversation-command: fork <hash>`.
pub(super) const MARKER: &str = "caos-conversation-command:";

/// The commit a prompt asks to fork, if it asks for one.
///
/// The slash form counts only as the FIRST line of the prompt: a prompt that
/// merely quotes one further down is not a request to fork anything. The marker
/// form may be anywhere, because it is text the command itself expanded to.
pub(super) fn parse_command(prompt: &str) -> Result<Option<Oid>, String> {
    let trimmed = prompt.trim_start();
    if let Some(first) = trimmed.lines().next().and_then(|l| l.strip_prefix('/')) {
        let mut words = first.split_whitespace();
        if words.next() == Some(FORK_COMMAND) {
            return commit(words.next()).map(Some);
        }
    }
    for line in prompt.lines() {
        let Some(rest) = line.trim().strip_prefix(MARKER) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        return match words.next() {
            Some("fork") => commit(words.next()).map(Some),
            other => Err(format!("unknown {MARKER} mode {other:?}")),
        };
    }
    Ok(None)
}

fn commit(hash: Option<&str>) -> Result<Oid, String> {
    let hash = hash.ok_or_else(|| format!("usage: /{FORK_COMMAND} <conversation-commit-hash>"))?;
    if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "/{FORK_COMMAND} takes a full 40-character commit hash (the one `caos head` printed), not {hash:?}"
        ));
    }
    oid(&hash.to_ascii_lowercase(), "conversation commit")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_slash_form_is_recognised_on_the_first_line() {
        let want = Some(commit(Some(HASH)).unwrap());
        assert_eq!(
            parse_command(&format!("  /{FORK_COMMAND}   {HASH}\nmore text")).unwrap(),
            want
        );
    }

    #[test]
    fn the_expanded_marker_is_recognised_anywhere() {
        let prompt = format!("Some preamble.\n{MARKER} fork {HASH}\nmore.");
        assert_eq!(
            parse_command(&prompt).unwrap(),
            Some(commit(Some(HASH)).unwrap())
        );
    }

    #[test]
    fn other_prompts_and_builtin_commands_are_not_ours() {
        assert_eq!(parse_command("fix the bug").unwrap(), None);
        // Claude Code's own /resume must not be mistaken for ours.
        assert_eq!(parse_command(&format!("/resume {HASH}")).unwrap(), None);
        assert_eq!(
            parse_command(&format!("/resume-caos-conversation {HASH}")).unwrap(),
            None
        );
        // Quoting a command further down is not asking for it.
        assert_eq!(
            parse_command(&format!("look:\n/{FORK_COMMAND} {HASH}")).unwrap(),
            None
        );
    }

    #[test]
    fn a_bad_hash_is_an_error_naming_the_command() {
        for prompt in [
            format!("/{FORK_COMMAND}"),
            format!("/{FORK_COMMAND} main"),
            format!("/{FORK_COMMAND} {}", &HASH[..12]),
        ] {
            let error = parse_command(&prompt).unwrap_err();
            assert!(error.contains("/fork-caos-conversation"));
        }
        assert!(parse_command(&format!("{MARKER} sideways {HASH}")).is_err());
        assert!(parse_command(&format!("{MARKER} resume {HASH}")).is_err());
    }
}
