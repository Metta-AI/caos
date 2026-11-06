#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    SessionRoot,
    SessionFork,
    MetadataTitleSet,
    MessageAppend,
    ToolStart,
    ToolComplete,
    AsyncStart,
    AsyncTerminal,
    SubagentSpawn,
    SubagentTerminal,
    PublicationPending,
    PublicationTerminal,
    FilesApply,
}

impl Kind {
    pub const ALL: [Kind; 13] = [
        Kind::SessionRoot,
        Kind::SessionFork,
        Kind::MetadataTitleSet,
        Kind::MessageAppend,
        Kind::ToolStart,
        Kind::ToolComplete,
        Kind::AsyncStart,
        Kind::AsyncTerminal,
        Kind::SubagentSpawn,
        Kind::SubagentTerminal,
        Kind::PublicationPending,
        Kind::PublicationTerminal,
        Kind::FilesApply,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SessionRoot => "session.root",
            Kind::SessionFork => "session.fork",
            Kind::MetadataTitleSet => "metadata.title.set",
            Kind::MessageAppend => "message.append",
            Kind::ToolStart => "tool.start",
            Kind::ToolComplete => "tool.complete",
            Kind::AsyncStart => "async.start",
            Kind::AsyncTerminal => "async.terminal",
            Kind::SubagentSpawn => "subagent.spawn",
            Kind::SubagentTerminal => "subagent.terminal",
            Kind::PublicationPending => "publication.pending",
            Kind::PublicationTerminal => "publication.terminal",
            Kind::FilesApply => "files.apply",
        }
    }

    pub fn parse(name: &str) -> Result<Kind, String> {
        // The names a v6 session's commits carry, from before the rename.
        match name {
            "conversation.root" => return Ok(Kind::SessionRoot),
            "conversation.fork" => return Ok(Kind::SessionFork),
            _ => {}
        }
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name)
            .ok_or_else(|| format!("unregistered session kind {name:?}"))
    }

    pub fn message(self) -> Vec<u8> {
        let mut message = self.as_str().as_bytes().to_vec();
        message.push(b'\n');
        message
    }

    pub fn parse_message(message: &[u8]) -> Result<Kind, String> {
        super::events::decode(message).map(|(kind, _)| kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_kinds_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Ok(kind));
            assert_eq!(
                Kind::parse_message(&super::super::events::encode(kind, &[])),
                Ok(kind)
            );
        }
    }

    #[test]
    fn v6_root_and_fork_names_still_parse() {
        assert_eq!(Kind::parse("conversation.root"), Ok(Kind::SessionRoot));
        assert_eq!(Kind::parse("conversation.fork"), Ok(Kind::SessionFork));
        let current = super::super::events::encode(Kind::SessionFork, &[]);
        let v6 = [
            b"conversation.fork".as_slice(),
            &current[b"session.fork".len()..],
        ]
        .concat();
        assert_eq!(Kind::parse_message(&v6), Ok(Kind::SessionFork));
        assert_eq!(Kind::SessionRoot.as_str(), "session.root");
    }

    #[test]
    fn commit_messages_are_exact() {
        for message in [
            b"tool.complete".as_slice(),
            b"tool.complete\n\n",
            b"Tool.complete\n",
            b"{\"kind\":\"tool.complete\"}\n",
        ] {
            assert!(Kind::parse_message(message).is_err());
        }
    }
}
