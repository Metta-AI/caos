use super::oid::{hex_lower, hex_nibble, is_lower_hex};

use super::writers::{self, Writer, NAMESPACE_PREFIX};

/// A conversation's refs live in a ref-writers namespace (design/ref-writers.md):
/// `refs/caos/w/<ns>/conversations/<hex id>/head`. A subagent's head sits in
/// its parent's namespace, so one writers list governs both.
pub const CONVERSATIONS_DIR: &str = "conversations";
/// A user's sidebar, in their personal namespace:
/// `refs/caos/w/<personal>/memberships/{active,archived}/<ns>/<hex id>`.
pub const MEMBERSHIPS_DIR: &str = "memberships";
pub const HEAD_SUFFIX: &str = "/head";
pub const MAX_CONVERSATION_ID_BYTES: usize = 124;
pub const MAX_USER_ID_BYTES: usize = 126;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    Active,
    Archived,
}

/// The namespace a writer's conversation `id` is created in: fixed by the key
/// and the id, so the creator never has to look it up.
pub fn conversation_namespace(key: &str, id: &str) -> Result<String, String> {
    validate_conversation_id(id)?;
    Ok(sole_writer_namespace(
        key,
        &format!("conversation {}", key_of(id)),
    ))
}

/// A writer's own namespace, holding their memberships.
pub fn personal_namespace(key: &str) -> String {
    sole_writer_namespace(key, "personal")
}

pub fn sole_writer(key: &str) -> Vec<Writer> {
    vec![Writer {
        key: key.to_string(),
        label: String::new(),
    }]
}

fn sole_writer_namespace(key: &str, label: &str) -> String {
    writers::genesis_id(&sole_writer(key), label).to_string()
}

pub fn validate_conversation_id(id: &str) -> Result<(), String> {
    validate_id(id, MAX_CONVERSATION_ID_BYTES, "conversation")
}

pub fn validate_user_id(id: &str) -> Result<(), String> {
    validate_id(id, MAX_USER_ID_BYTES, "user")
}

pub fn key_of(id: &str) -> String {
    hex_lower(id.as_bytes())
}

pub fn id_of_key(key: &str) -> Result<String, String> {
    if !key.len().is_multiple_of(2) || !is_lower_hex(key) {
        return Err(format!("invalid lowercase hexadecimal id key {key:?}"));
    }
    let mut bytes = Vec::with_capacity(key.len() / 2);
    for pair in key.as_bytes().as_chunks::<2>().0 {
        bytes.push((hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]));
    }
    let id = String::from_utf8(bytes).map_err(|_| format!("id key is not UTF-8: {key:?}"))?;
    if key_of(&id) != key {
        return Err(format!("non-canonical id key {key:?}"));
    }
    Ok(id)
}

pub fn head_ref(namespace: &str, id: &str) -> Result<String, String> {
    writers::validate_namespace(namespace)?;
    validate_conversation_id(id)?;
    Ok(format!(
        "{NAMESPACE_PREFIX}{namespace}/{CONVERSATIONS_DIR}/{}{HEAD_SUFFIX}",
        key_of(id)
    ))
}

/// A conversation's address, `<namespace>/<id>`: what names it across
/// namespaces, e.g. in a secret's `reader:@=<path> conversation=<address>`.
pub fn address(namespace: &str, id: &str) -> String {
    format!("{namespace}/{id}")
}

pub fn parse_address(address: &str) -> Result<(String, String), String> {
    let (namespace, id) = address
        .split_once('/')
        .filter(|(namespace, _)| writers::is_namespace(namespace))
        .ok_or_else(|| format!("{address:?} is not a conversation address (<namespace>/<id>)"))?;
    validate_conversation_id(id)?;
    Ok((namespace.to_string(), id.to_string()))
}

/// Every conversation head named `id`, in any namespace: a pattern for
/// `git ls-remote`.
pub fn head_ref_pattern(id: &str) -> Result<String, String> {
    validate_conversation_id(id)?;
    Ok(format!(
        "{NAMESPACE_PREFIX}*/{CONVERSATIONS_DIR}/{}{HEAD_SUFFIX}",
        key_of(id)
    ))
}

/// Every conversation head, in any namespace.
pub const ALL_HEADS_PATTERN: &str = "refs/caos/w/*/conversations/*/head";

pub fn active_membership_ref(personal: &str, namespace: &str, id: &str) -> Result<String, String> {
    membership_ref(personal, Membership::Active, namespace, id)
}

pub fn archived_membership_ref(
    personal: &str,
    namespace: &str,
    id: &str,
) -> Result<String, String> {
    membership_ref(personal, Membership::Archived, namespace, id)
}

/// `(namespace, id)` of a conversation head ref.
pub fn parse_head_ref(refname: &str) -> Result<(String, String), String> {
    let invalid = || format!("invalid conversation head ref {refname:?}");
    let (namespace, rest) = writers::split_ref(refname).ok_or_else(invalid)?;
    let key = rest
        .strip_prefix(CONVERSATIONS_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_suffix(HEAD_SUFFIX))
        .ok_or_else(invalid)?;
    if key.contains('/') {
        return Err(invalid());
    }
    let id = id_of_key(key).map_err(|_| invalid())?;
    validate_conversation_id(&id).map_err(|_| invalid())?;
    Ok((namespace.to_string(), id))
}

/// `(personal namespace, membership, conversation namespace, id)`.
pub fn parse_membership_ref(refname: &str) -> Result<(String, Membership, String, String), String> {
    let invalid = || format!("invalid conversation membership ref {refname:?}");
    let (personal, rest) = writers::split_ref(refname).ok_or_else(invalid)?;
    let rest = rest
        .strip_prefix(MEMBERSHIPS_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(invalid)?;
    let (membership, rest) = if let Some(rest) = rest.strip_prefix("active/") {
        (Membership::Active, rest)
    } else if let Some(rest) = rest.strip_prefix("archived/") {
        (Membership::Archived, rest)
    } else {
        return Err(invalid());
    };
    let (namespace, key) = rest.split_once('/').ok_or_else(invalid)?;
    if !writers::is_namespace(namespace) || key.contains('/') {
        return Err(invalid());
    }
    let id = id_of_key(key).map_err(|_| invalid())?;
    validate_conversation_id(&id).map_err(|_| invalid())?;
    Ok((personal.to_string(), membership, namespace.to_string(), id))
}

/// The prefix under which a personal namespace lists memberships.
pub fn memberships_prefix(personal: &str) -> Result<String, String> {
    writers::validate_namespace(personal)?;
    Ok(format!("{NAMESPACE_PREFIX}{personal}/{MEMBERSHIPS_DIR}/"))
}

fn validate_id(id: &str, maximum: usize, kind: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > maximum || id.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(format!("invalid {kind} id {id:?}"));
    }
    Ok(())
}

fn membership_ref(
    personal: &str,
    membership: Membership,
    namespace: &str,
    id: &str,
) -> Result<String, String> {
    writers::validate_namespace(namespace)?;
    validate_conversation_id(id)?;
    let kind = match membership {
        Membership::Active => "active",
        Membership::Archived => "archived",
    };
    Ok(format!(
        "{}{kind}/{namespace}/{}",
        memberships_prefix(personal)?,
        key_of(id)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29";

    #[test]
    fn head_refs_round_trip() {
        let ns = conversation_namespace(KEY, "talk-1").unwrap();
        let refname = head_ref(&ns, "talk-1").unwrap();
        assert_eq!(
            refname,
            format!("refs/caos/w/{ns}/conversations/74616c6b2d31/head")
        );
        assert_eq!(
            parse_head_ref(&refname).unwrap(),
            (ns, "talk-1".to_string())
        );
        assert!(parse_head_ref("refs/caos/v3/conversations/74616c6b2d31/head").is_err());
    }

    #[test]
    fn namespaces_are_fixed_by_key_and_id() {
        let a = conversation_namespace(KEY, "talk-1").unwrap();
        assert_eq!(a, conversation_namespace(KEY, "talk-1").unwrap());
        assert_ne!(a, conversation_namespace(KEY, "talk-2").unwrap());
        assert_ne!(a, personal_namespace(KEY));
    }

    #[test]
    fn membership_refs_round_trip() {
        let personal = personal_namespace(KEY);
        let ns = conversation_namespace(KEY, "talk-1").unwrap();
        for (refname, membership) in [
            (
                active_membership_ref(&personal, &ns, "talk-1").unwrap(),
                Membership::Active,
            ),
            (
                archived_membership_ref(&personal, &ns, "talk-1").unwrap(),
                Membership::Archived,
            ),
        ] {
            assert_eq!(
                parse_membership_ref(&refname),
                Ok((
                    personal.clone(),
                    membership,
                    ns.clone(),
                    "talk-1".to_string()
                ))
            );
        }
    }

    #[test]
    fn invalid_ids_and_keys_are_rejected() {
        for id in ["", "control\n", "delete\u{7f}"] {
            assert!(validate_conversation_id(id).is_err());
            assert!(validate_user_id(id).is_err());
        }
        assert!(validate_conversation_id(&"a".repeat(MAX_CONVERSATION_ID_BYTES + 1)).is_err());
        assert!(validate_user_id(&"a".repeat(MAX_USER_ID_BYTES + 1)).is_err());
        for key in ["0", "AA", "gg", "ff"] {
            assert!(id_of_key(key).is_err(), "accepted {key:?}");
        }
    }
}
