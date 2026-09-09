//! TUI command-line arguments.

use caos_cli::{missing_image_arg, normalized_username, TurnOptions, LLM_CALL_ARG, LLM_STEP_ARG};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) user: String,
    pub(crate) list_archived: bool,
    pub(crate) unarchive: Option<String>,
    pub(crate) conversation: Option<String>,
    pub(crate) new_conversation: bool,
    pub(crate) from_commit: Option<String>,
    pub(crate) turn: TurnOptions,
}

impl Args {
    pub(crate) fn parse(raw: &[String]) -> Result<Self, String> {
        let default_user = match std::env::var("USER") {
            Ok(user) => Some(user),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) if raw.iter().any(|arg| arg == "--username") => {
                None
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err("$USER is not valid UTF-8; pass --username explicitly".to_string())
            }
        };
        Self::parse_with_default_user(raw, default_user)
    }

    /// `default_user` is only consulted when `--username` is absent, so an
    /// explicit identity works (and tests run) without `$USER`.
    fn parse_with_default_user(
        raw: &[String],
        default_user: Option<String>,
    ) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut user_flag: Option<String> = None;
        let mut args = raw.iter();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::slice::Iter<'_, String>, flag: &str| {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))
            };
            match arg.as_str() {
                "--username" => user_flag = Some(value(&mut args, arg)?),
                "--list-archived" => parsed.list_archived = true,
                "--unarchive" => parsed.unarchive = Some(value(&mut args, arg)?),
                "-c" | "--conversation" => parsed.conversation = Some(value(&mut args, arg)?),
                "--new" => parsed.new_conversation = true,
                "--from" => parsed.from_commit = Some(value(&mut args, arg)?),
                "--base" => parsed.turn.base = Some(value(&mut args, arg)?),
                "--system" => parsed.turn.system = Some(value(&mut args, arg)?),
                "--system-file" => parsed.turn.system_file = Some(value(&mut args, arg)?),
                "--model" => parsed.turn.model = Some(value(&mut args, arg)?),
                "--base-url" => parsed.turn.base_url = Some(value(&mut args, arg)?),
                "-h" | "--help" => return Err(usage()),
                other if parsed.turn.take_image_arg(other) => {}
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
        }
        parsed.user = match user_flag {
            Some(user) => normalized_username(&user).ok_or_else(|| {
                "--username must be 1-126 UTF-8 bytes and contain no control or invisible formatting characters"
                    .to_string()
            })?,
            None => {
                let user = default_user
                    .ok_or_else(|| "--username is required when $USER is not set".to_string())?;
                normalized_username(&user).ok_or_else(|| {
                    "$USER is not a usable identity; pass --username explicitly".to_string()
                })?
            }
        };
        if parsed.turn.system.is_some() && parsed.turn.system_file.is_some() {
            return Err("--system and --system-file are mutually exclusive".to_string());
        }
        if parsed.from_commit.is_some() && parsed.turn.base.is_some() {
            return Err("--from and --base are mutually exclusive".to_string());
        }
        if parsed.from_commit.is_some() && parsed.conversation.is_some() {
            return Err(
                "--from starts a fresh conversation and cannot be combined with -c".to_string(),
            );
        }
        if let Some(from) = &parsed.from_commit {
            parsed.new_conversation = true;
            parsed.turn.base = Some(from.clone());
        }
        if parsed.list_archived && parsed.unarchive.is_some() {
            return Err("--list-archived and --unarchive are mutually exclusive".to_string());
        }
        if (parsed.list_archived || parsed.unarchive.is_some())
            && (parsed.conversation.is_some()
                || parsed.new_conversation
                || parsed.from_commit.is_some()
                || parsed.turn != TurnOptions::default())
        {
            return Err(
                "archive-management options cannot be combined with conversation options"
                    .to_string(),
            );
        }
        // A session that only lists or restores archives touches no worker, and
        // the check above already forbids combining those with turn options —
        // so the images are required for everything else, and required HERE so
        // the message reaches the shell rather than the alternate screen.
        if !parsed.list_archived && parsed.unarchive.is_none() {
            for (argument, name) in [
                (&parsed.turn.llm_step, LLM_STEP_ARG),
                (&parsed.turn.llm_call, LLM_CALL_ARG),
            ] {
                if argument.is_none() {
                    return Err(format!("{}\n{}", missing_image_arg(name), usage()));
                }
            }
        }
        parsed.turn.username = Some(parsed.user.clone());
        Ok(parsed)
    }
}

pub(crate) fn usage() -> String {
    "usage: caos tui --llm-step:@=<path> --llm-call:@=<path> [--username <name>] \
     [--list-archived | --unarchive <conversation-id>] \
     [--new | --from <commit>] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>]\n\
     \x20 the two image args also take :@@=<git ref>, :hash=<oid> and :docker=<ref>"
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::Args;

    /// A conversation session names its two workers, so every parse that is
    /// meant to SUCCEED carries them. `app.rs` uses this too.
    pub(crate) fn with_images(raw: &[&str]) -> Vec<String> {
        ["--llm-step:@=std/llm-step", "--llm-call:@=std/llm-call"]
            .iter()
            .chain(raw)
            .map(|argument| (*argument).to_string())
            .collect()
    }

    #[test]
    fn username_is_the_one_user_identity() {
        let default =
            Args::parse_with_default_user(&with_images(&[]), Some("alice".to_string())).unwrap();
        assert_eq!(default.user, "alice");

        let explicit = Args::parse_with_default_user(
            &with_images(&["--username", "Bob"]),
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(explicit.user, "Bob");
        assert_eq!(explicit.turn.username.as_deref(), Some("Bob"));

        let normalized = Args::parse_with_default_user(
            &with_images(&["--username", "  Alice Smith  "]),
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(normalized.user, "Alice Smith");
        assert_eq!(normalized.turn.username.as_deref(), Some("Alice Smith"));

        let no_ambient =
            Args::parse_with_default_user(&with_images(&["--username", "bob"]), None).unwrap();
        assert_eq!(no_ambient.user, "bob");

        assert!(Args::parse_with_default_user(&with_images(&[]), None).is_err());
        assert!(Args::parse_with_default_user(
            &with_images(&["--username", " \t "]),
            Some("alice".to_string()),
        )
        .is_err());
        assert!(Args::parse_with_default_user(
            &with_images(&["--username", "alice\nbob"]),
            Some("alice".to_string()),
        )
        .is_err());
        assert!(Args::parse_with_default_user(
            &with_images(&["--username", "ali\u{200b}ce"]),
            Some("alice".to_string()),
        )
        .is_err());

        let ambient_error =
            Args::parse_with_default_user(&with_images(&[]), Some(" \t ".to_string()))
                .expect_err("an unusable ambient identity was accepted");
        assert!(ambient_error.contains("$USER"), "{ambient_error}");
        assert!(ambient_error.contains("--username"), "{ambient_error}");
    }

    #[test]
    fn a_conversation_names_its_workers_and_an_archive_listing_does_not() {
        let named = Args::parse_with_default_user(
            &with_images(&["--username", "alice"]),
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(
            named.turn.llm_step.as_deref(),
            Some("--llm-step:@=std/llm-step")
        );
        assert_eq!(
            named.turn.llm_call.as_deref(),
            Some("--llm-call:@=std/llm-call")
        );

        // Every arg type the vocabulary has, not just `:@=`.
        let pinned = Args::parse_with_default_user(
            &[
                "--llm-step:@@=git+https://example.invalid/caos?rev=abc&dir=std/llm-step"
                    .to_string(),
                "--llm-call:hash=0123456789abcdef".to_string(),
            ],
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(
            pinned.turn.llm_call.as_deref(),
            Some("--llm-call:hash=0123456789abcdef")
        );

        // Absent, the message names the flag and both spellings.
        let missing = Args::parse_with_default_user(
            &["--llm-step:@=std/llm-step".to_string()],
            Some("alice".to_string()),
        )
        .expect_err("a conversation ran without its llm-call image");
        assert!(missing.contains("--llm-call:@="), "{missing}");
        assert!(missing.contains("caos-std/llm-call"), "{missing}");

        // An archive listing runs no worker, so it needs neither — and
        // combining the two is already refused as a turn option.
        assert!(Args::parse_with_default_user(
            &["--list-archived".to_string()],
            Some("alice".to_string()),
        )
        .is_ok());
        assert!(Args::parse_with_default_user(
            &with_images(&["--list-archived"]),
            Some("alice".to_string()),
        )
        .is_err());
    }

    #[test]
    fn archive_management_is_non_interactive_and_exclusive() {
        let list = Args::parse_with_default_user(
            &["--list-archived".to_string()],
            Some("alice".to_string()),
        )
        .unwrap();
        assert!(list.list_archived);

        let restore = Args::parse_with_default_user(
            &["--unarchive".to_string(), "abc123".to_string()],
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(restore.unarchive.as_deref(), Some("abc123"));

        assert!(Args::parse_with_default_user(
            &["--list-archived".to_string(), "--new".to_string(),],
            Some("alice".to_string()),
        )
        .is_err());
    }
}
