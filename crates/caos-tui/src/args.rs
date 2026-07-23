use caos::chat::TurnOptions;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) conversation: Option<String>,
    pub(crate) new_conversation: bool,
    pub(crate) from_commit: Option<String>,
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
                "--from" => parsed.from_commit = Some(value(&mut args, arg)?),
                "--base" => parsed.turn.base = Some(value(&mut args, arg)?),
                "--system" => parsed.turn.system = Some(value(&mut args, arg)?),
                "--system-file" => parsed.turn.system_file = Some(value(&mut args, arg)?),
                "--model" => parsed.turn.model = Some(value(&mut args, arg)?),
                "--base-url" => parsed.turn.base_url = Some(value(&mut args, arg)?),
                "--llm-step-bin" => parsed.turn.llm_step_bin = Some(value(&mut args, arg)?),
                "--bash-tool-bin" => parsed.turn.bash_tool_bin = Some(value(&mut args, arg)?),
                "--rgrep-bin" => parsed.turn.rgrep_bin = Some(value(&mut args, arg)?),
                "--tools" => parsed.turn.tools = Some(value(&mut args, arg)?),
                "-h" | "--help" => return Err(usage()),
                other => return Err(format!("unknown option {other:?}\n{}", usage())),
            }
        }
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
        Ok(parsed)
    }
}

pub(crate) fn usage() -> String {
    "usage: caos-tui [--new | --from <commit>] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>] \
     [--tools <path>]"
        .to_string()
}
