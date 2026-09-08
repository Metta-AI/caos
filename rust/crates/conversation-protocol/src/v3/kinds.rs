#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    ConversationRoot,
    ConversationFork,
    MetadataTitleSet,
    MessageAppend,
    RequestAdmit,
    RequestClaim,
    RequestInterject,
    RequestEscape,
    RequestTerminal,
    ModelComplete,
    ToolStart,
    ToolComplete,
    AsyncStart,
    AsyncTerminal,
    SubagentSpawn,
    SubagentTerminal,
    SubagentApply,
    WorkspaceCreate,
    WorkspaceConfigure,
    WorkspaceAdvance,
    WorkspaceRollback,
    WorkspaceRemove,
    PublicationPending,
    PublicationTerminal,
    FilesApply,
}

impl Kind {
    pub const ALL: [Kind; 25] = [
        Kind::ConversationRoot,
        Kind::ConversationFork,
        Kind::MetadataTitleSet,
        Kind::MessageAppend,
        Kind::RequestAdmit,
        Kind::RequestClaim,
        Kind::RequestInterject,
        Kind::RequestEscape,
        Kind::RequestTerminal,
        Kind::ModelComplete,
        Kind::ToolStart,
        Kind::ToolComplete,
        Kind::AsyncStart,
        Kind::AsyncTerminal,
        Kind::SubagentSpawn,
        Kind::SubagentTerminal,
        Kind::SubagentApply,
        Kind::WorkspaceCreate,
        Kind::WorkspaceConfigure,
        Kind::WorkspaceAdvance,
        Kind::WorkspaceRollback,
        Kind::WorkspaceRemove,
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
            Kind::RequestAdmit => "request.admit",
            Kind::RequestClaim => "request.claim",
            Kind::RequestInterject => "request.interject",
            Kind::RequestEscape => "request.escape",
            Kind::RequestTerminal => "request.terminal",
            Kind::ModelComplete => "model.complete",
            Kind::ToolStart => "tool.start",
            Kind::ToolComplete => "tool.complete",
            Kind::AsyncStart => "async.start",
            Kind::AsyncTerminal => "async.terminal",
            Kind::SubagentSpawn => "subagent.spawn",
            Kind::SubagentTerminal => "subagent.terminal",
            Kind::SubagentApply => "subagent.apply",
            Kind::WorkspaceCreate => "workspace.create",
            Kind::WorkspaceConfigure => "workspace.configure",
            Kind::WorkspaceAdvance => "workspace.advance",
            Kind::WorkspaceRollback => "workspace.rollback",
            Kind::WorkspaceRemove => "workspace.remove",
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
        let parsed = message
            .strip_suffix(b"\n")
            .and_then(|name| std::str::from_utf8(name).ok())
            .and_then(|name| Self::parse(name).ok());
        match parsed {
            Some(kind) if kind.message() == message => Ok(kind),
            _ => Err(format!(
                "commit message is not a registered kind: {message:?}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_kinds_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Ok(kind));
            assert_eq!(Kind::parse_message(&kind.message()), Ok(kind));
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
