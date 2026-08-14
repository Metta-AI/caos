//! TUI command-line arguments.

use caos::chat::{normalized_username, TurnOptions};

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
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
        }
        parsed.user = match user_flag {
            Some(user) => normalized_username(&user).ok_or_else(|| {
                "--username must be nonempty and contain no control or invisible formatting characters"
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
        parsed.turn.username = Some(parsed.user.clone());
        Ok(parsed)
    }
}

pub(crate) fn usage() -> String {
    "usage: caos tui [--username <name>] [--list-archived | --unarchive <conversation-id>] \
     [--new | --from <commit>] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn username_is_the_one_user_identity() {
        let default = Args::parse_with_default_user(&[], Some("alice".to_string())).unwrap();
        assert_eq!(default.user, "alice");

        let explicit = Args::parse_with_default_user(
            &["--username".to_string(), "Bob".to_string()],
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(explicit.user, "Bob");
        assert_eq!(explicit.turn.username.as_deref(), Some("Bob"));

        let normalized = Args::parse_with_default_user(
            &["--username".to_string(), "  Alice Smith  ".to_string()],
            Some("alice".to_string()),
        )
        .unwrap();
        assert_eq!(normalized.user, "Alice Smith");
        assert_eq!(normalized.turn.username.as_deref(), Some("Alice Smith"));

        let no_ambient =
            Args::parse_with_default_user(&["--username".to_string(), "bob".to_string()], None)
                .unwrap();
        assert_eq!(no_ambient.user, "bob");

        assert!(Args::parse_with_default_user(&[], None).is_err());
        assert!(Args::parse_with_default_user(
            &["--username".to_string(), " \t ".to_string()],
            Some("alice".to_string()),
        )
        .is_err());
        assert!(Args::parse_with_default_user(
            &["--username".to_string(), "alice\nbob".to_string()],
            Some("alice".to_string()),
        )
        .is_err());
        assert!(Args::parse_with_default_user(
            &["--username".to_string(), "ali\u{200b}ce".to_string()],
            Some("alice".to_string()),
        )
        .is_err());

        let ambient_error = Args::parse_with_default_user(&[], Some(" \t ".to_string()))
            .expect_err("an unusable ambient identity was accepted");
        assert!(ambient_error.contains("$USER"), "{ambient_error}");
        assert!(ambient_error.contains("--username"), "{ambient_error}");
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
