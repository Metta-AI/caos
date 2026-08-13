//! Command-line arguments for the deliberately small conversation TUI.

use caos::chat::TurnOptions;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) conversation: Option<String>,
    pub(crate) new_conversation: bool,
    pub(crate) turn: TurnOptions,
}

impl Args {
    pub(crate) fn parse(raw: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = raw.iter();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::slice::Iter<'_, String>, flag: &str| {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))
            };
            match arg.as_str() {
                "-c" | "--conversation" => parsed.conversation = Some(value(&mut args, arg)?),
                "--new" => parsed.new_conversation = true,
                "--base" => parsed.turn.base = Some(value(&mut args, arg)?),
                "--system" => parsed.turn.system = Some(value(&mut args, arg)?),
                "--system-file" => parsed.turn.system_file = Some(value(&mut args, arg)?),
                "--model" => parsed.turn.model = Some(value(&mut args, arg)?),
                "--base-url" => parsed.turn.base_url = Some(value(&mut args, arg)?),
                "-h" | "--help" => return Err(usage()),
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
        }
        if parsed.turn.system.is_some() && parsed.turn.system_file.is_some() {
            return Err("--system and --system-file are mutually exclusive".to_string());
        }
        Ok(parsed)
    }
}

pub(crate) fn usage() -> String {
    "usage: caos tui [-c <conversation-id>] [--new] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn parses_the_small_conversation_surface() {
        let args = Args::parse(&[
            "--conversation".to_string(),
            "chat-7".to_string(),
            "--new".to_string(),
            "--base".to_string(),
            "main".to_string(),
            "--model".to_string(),
            "test-model".to_string(),
        ])
        .unwrap();

        assert_eq!(args.conversation.as_deref(), Some("chat-7"));
        assert!(args.new_conversation);
        assert_eq!(args.turn.base.as_deref(), Some("main"));
        assert_eq!(args.turn.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn rejects_ambiguous_system_prompts_and_removed_options() {
        assert!(Args::parse(&[
            "--system".to_string(),
            "inline".to_string(),
            "--system-file".to_string(),
            "prompt.txt".to_string(),
        ])
        .is_err());
        assert!(Args::parse(&["--list-archived".to_string()]).is_err());
    }
}
