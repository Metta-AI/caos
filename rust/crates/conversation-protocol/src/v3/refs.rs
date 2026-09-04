use super::oid::{hex_lower, hex_nibble, is_lower_hex};

pub const CONVERSATIONS_PREFIX: &str = "refs/caos/v3/conversations/";
pub const USERS_PREFIX: &str = "refs/caos/v3/users/";
pub const HEAD_SUFFIX: &str = "/head";
pub const MAX_CONVERSATION_ID_BYTES: usize = 124;
pub const MAX_USER_ID_BYTES: usize = 126;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    Active,
    Archived,
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
    for pair in key.as_bytes().chunks_exact(2) {
        bytes.push((hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]));
    }
    let id = String::from_utf8(bytes).map_err(|_| format!("id key is not UTF-8: {key:?}"))?;
    if key_of(&id) != key {
        return Err(format!("non-canonical id key {key:?}"));
    }
    Ok(id)
}

pub fn head_ref(id: &str) -> Result<String, String> {
    validate_conversation_id(id)?;
    Ok(format!("{CONVERSATIONS_PREFIX}{}{HEAD_SUFFIX}", key_of(id)))
}

pub fn active_membership_ref(user: &str, id: &str) -> Result<String, String> {
    membership_ref(user, Membership::Active, id)
}

pub fn archived_membership_ref(user: &str, id: &str) -> Result<String, String> {
    membership_ref(user, Membership::Archived, id)
}

pub fn parse_head_ref(refname: &str) -> Result<String, String> {
    let key = refname
        .strip_prefix(CONVERSATIONS_PREFIX)
        .and_then(|rest| rest.strip_suffix(HEAD_SUFFIX))
        .ok_or_else(|| format!("invalid conversation head ref {refname:?}"))?;
    if key.contains('/') {
        return Err(format!("invalid conversation head ref {refname:?}"));
    }
    let id = id_of_key(key).map_err(|_| format!("invalid conversation head ref {refname:?}"))?;
    validate_conversation_id(&id)
        .map_err(|_| format!("invalid conversation head ref {refname:?}"))?;
    Ok(id)
}

pub fn parse_membership_ref(refname: &str) -> Result<(String, Membership, String), String> {
    let rest = refname
        .strip_prefix(USERS_PREFIX)
        .ok_or_else(|| format!("invalid conversation membership ref {refname:?}"))?;
    let (user_key, rest) = rest
        .split_once("/conversations/")
        .ok_or_else(|| format!("invalid conversation membership ref {refname:?}"))?;
    let (membership, conversation_key) = if let Some(key) = rest.strip_prefix("active/") {
        (Membership::Active, key)
    } else if let Some(key) = rest.strip_prefix("archived/") {
        (Membership::Archived, key)
    } else {
        return Err(format!("invalid conversation membership ref {refname:?}"));
    };
    if user_key.contains('/') || conversation_key.contains('/') {
        return Err(format!("invalid conversation membership ref {refname:?}"));
    }
    let user = id_of_key(user_key)
        .map_err(|_| format!("invalid conversation membership ref {refname:?}"))?;
    let conversation = id_of_key(conversation_key)
        .map_err(|_| format!("invalid conversation membership ref {refname:?}"))?;
    validate_user_id(&user)
        .and_then(|()| validate_conversation_id(&conversation))
        .map_err(|_| format!("invalid conversation membership ref {refname:?}"))?;
    Ok((user, membership, conversation))
}

fn validate_id(id: &str, maximum: usize, kind: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > maximum || id.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(format!("invalid {kind} id {id:?}"));
    }
    Ok(())
}

fn membership_ref(user: &str, membership: Membership, id: &str) -> Result<String, String> {
    validate_user_id(user)?;
    validate_conversation_id(id)?;
    let kind = match membership {
        Membership::Active => "active",
        Membership::Archived => "archived",
    };
    Ok(format!(
        "{USERS_PREFIX}{}/conversations/{kind}/{}",
        key_of(user),
        key_of(id)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_refs_round_trip() {
        let refname = head_ref("talk-1").unwrap();
        assert_eq!(refname, "refs/caos/v3/conversations/74616c6b2d31/head");
        assert_eq!(parse_head_ref(&refname).unwrap(), "talk-1");
    }

    #[test]
    fn membership_refs_round_trip() {
        let active = active_membership_ref("user@example", "talk-1").unwrap();
        assert_eq!(
            parse_membership_ref(&active),
            Ok((
                "user@example".to_string(),
                Membership::Active,
                "talk-1".to_string()
            ))
        );
        let archived = archived_membership_ref("user@example", "talk-1").unwrap();
        assert_eq!(
            parse_membership_ref(&archived),
            Ok((
                "user@example".to_string(),
                Membership::Archived,
                "talk-1".to_string()
            ))
        );
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
