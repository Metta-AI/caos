//! Pure validation and naming rules for the append-only conversation log.
//!
//! Host and worker crates share this module so every process accepts exactly
//! the same conversation IDs, object IDs, and event-spine boundaries.

use serde_json::Value;

pub const CONVERSATION_PREFIX: &str = "refs/caos/v2/conversations/";
pub const CONVERSATION_HEAD_SUFFIX: &str = "/head";
pub const MAX_CONVERSATION_ID_BYTES: usize = 124;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectId<'a>(&'a str);

impl<'a> ObjectId<'a> {
    pub fn parse(value: &'a str, what: &str) -> Result<Self, String> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(format!(
                "{what} must be a lowercase 40-character hexadecimal hash, got {value:?}"
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(self) -> &'a str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationId<'a>(&'a str);

impl<'a> ConversationId<'a> {
    pub fn parse(value: &'a str) -> Result<Self, String> {
        if value.is_empty()
            || value.len() > MAX_CONVERSATION_ID_BYTES
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains("//")
            || value.contains("..")
            || value.contains("@{")
            || value.ends_with('.')
            || value.bytes().any(|byte| {
                byte <= b' '
                    || byte == 0x7f
                    || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
            })
            || value.split('/').any(|component| {
                component.is_empty()
                    || component == "."
                    || matches!(component, "head" | "title")
                    || component.starts_with('.')
                    || component.ends_with(".lock")
            })
        {
            return Err(format!("invalid conversation id {value:?}"));
        }
        Ok(Self(value))
    }

    pub fn as_str(self) -> &'a str {
        self.0
    }

    pub fn head_ref(self) -> String {
        format!("{CONVERSATION_PREFIX}{}{CONVERSATION_HEAD_SUFFIX}", self.0)
    }
}

pub fn conversation_ref(id: &str) -> Result<String, String> {
    ConversationId::parse(id).map(ConversationId::head_ref)
}

pub fn validate_conversation_ref(refname: &str) -> Result<(), String> {
    let Some(id) = refname
        .strip_prefix(CONVERSATION_PREFIX)
        .and_then(|rest| rest.strip_suffix(CONVERSATION_HEAD_SUFFIX))
    else {
        return Err(format!("invalid conversation head ref {refname:?}"));
    };
    ConversationId::parse(id)
        .map(|_| ())
        .map_err(|_| format!("invalid conversation head ref {refname:?}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventBoundary {
    Ordinary,
    Root,
    Fork,
}

#[derive(Clone, Copy, Debug)]
pub struct ConversationEvent<'a>(&'a Value);

impl<'a> ConversationEvent<'a> {
    pub fn parse(value: &'a Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "conversation event must be a JSON object".to_string())?;
        if object.contains_key("v") {
            return Err(
                "conversation event must not carry a version; refs/caos/v2 selects the protocol"
                    .to_string(),
            );
        }
        Ok(Self(value))
    }

    pub fn parse_append(value: &'a Value) -> Result<Self, String> {
        let event = Self::parse(value)?;
        if value.get("base").is_some() {
            return Err("an append event must not introduce a conversation base".to_string());
        }
        if value.get("forked_from").is_some() {
            return Err("a worker append must not introduce a conversation fork".to_string());
        }
        Ok(event)
    }

    pub fn boundary(self, first_parent: &str) -> Result<EventBoundary, String> {
        ObjectId::parse(first_parent, "conversation event parent")?;
        let base = self.0.get("base");
        let forked_from = self.0.get("forked_from");
        if base.is_some() && forked_from.is_some() {
            return Err("a conversation fork marker must not introduce a new base".to_string());
        }
        if let Some(base) = base {
            let base = base
                .as_str()
                .ok_or_else(|| "conversation event base is not a string".to_string())?;
            ObjectId::parse(base, "conversation base")?;
            if base != first_parent {
                return Err(format!(
                    "conversation root declares base {base}, but its first parent is {first_parent}"
                ));
            }
            return Ok(EventBoundary::Root);
        }
        if let Some(forked_from) = forked_from {
            let forked_from = forked_from
                .as_str()
                .ok_or_else(|| "conversation event forked_from is not a string".to_string())?;
            ObjectId::parse(forked_from, "forked_from")?;
            if forked_from != first_parent {
                return Err(format!(
                    "conversation fork marker declares {forked_from}, but its first parent is {first_parent}"
                ));
            }
            return Ok(EventBoundary::Fork);
        }
        Ok(EventBoundary::Ordinary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn object_ids_are_canonical_lowercase_sha1s() {
        assert_eq!(ObjectId::parse(A, "object").unwrap().as_str(), A);
        for invalid in [
            "a",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            B.trim_end_matches('b'),
        ] {
            assert!(ObjectId::parse(invalid, "object").is_err());
        }
    }

    #[test]
    fn conversation_ids_produce_one_canonical_ref() {
        assert_eq!(
            ConversationId::parse("project/talk-1").unwrap().head_ref(),
            "refs/caos/v2/conversations/project/talk-1/head"
        );
        assert!(ConversationId::parse(&"a".repeat(MAX_CONVERSATION_ID_BYTES)).is_ok());
        for invalid in [
            "",
            "/talk",
            "talk/",
            "talk//now",
            "talk..now",
            "talk@{now",
            "talk.",
            ".hidden",
            "a/.hidden",
            "a/head/b",
            "a/title/b",
            "a/topic.lock",
            "white space",
        ] {
            assert!(
                ConversationId::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(ConversationId::parse(&"a".repeat(MAX_CONVERSATION_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn event_boundaries_are_explicit_and_exclusive() {
        assert_eq!(
            ConversationEvent::parse(&json!({})).unwrap().boundary(A),
            Ok(EventBoundary::Ordinary)
        );
        assert_eq!(
            ConversationEvent::parse(&json!({"base": A}))
                .unwrap()
                .boundary(A),
            Ok(EventBoundary::Root)
        );
        assert_eq!(
            ConversationEvent::parse(&json!({"forked_from": A}))
                .unwrap()
                .boundary(A),
            Ok(EventBoundary::Fork)
        );
        assert!(ConversationEvent::parse(&json!({"base": B}))
            .unwrap()
            .boundary(A)
            .is_err());
        assert!(
            ConversationEvent::parse(&json!({"base": A, "forked_from": A}))
                .unwrap()
                .boundary(A)
                .is_err()
        );
    }

    #[test]
    fn append_events_cannot_change_the_spine_boundary() {
        assert!(ConversationEvent::parse_append(&json!({"status": "idle"})).is_ok());
        assert!(ConversationEvent::parse_append(&json!({"base": A})).is_err());
        assert!(ConversationEvent::parse_append(&json!({"forked_from": A})).is_err());
        assert!(ConversationEvent::parse_append(&json!({"v": 2})).is_err());
        assert!(ConversationEvent::parse_append(&json!([])).is_err());
    }
}
