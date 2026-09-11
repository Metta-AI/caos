#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    ConversationRoot,
    ConversationFork,
    MetadataTitleSet,
    MessageAppend,
    TurnAdmit,
    TurnClaim,
    TurnInterject,
    TurnEscape,
    TurnTerminal,
    ModelComplete,
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
    pub const ALL: [Kind; 19] = [
        Kind::ConversationRoot,
        Kind::ConversationFork,
        Kind::MetadataTitleSet,
        Kind::MessageAppend,
        Kind::TurnAdmit,
        Kind::TurnClaim,
        Kind::TurnInterject,
        Kind::TurnEscape,
        Kind::TurnTerminal,
        Kind::ModelComplete,
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
            Kind::ConversationRoot => "conversation.root",
            Kind::ConversationFork => "conversation.fork",
            Kind::MetadataTitleSet => "metadata.title.set",
            Kind::MessageAppend => "message.append",
            Kind::TurnAdmit => "request.admit",
            Kind::TurnClaim => "request.claim",
            Kind::TurnInterject => "request.interject",
            Kind::TurnEscape => "request.escape",
            Kind::TurnTerminal => "request.terminal",
            Kind::ModelComplete => "model.complete",
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
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name)
            .ok_or_else(|| format!("unregistered conversation kind {name:?}"))
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
