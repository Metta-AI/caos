use super::oid::{hex_lower, hex_nibble, is_lower_hex};

pub const SESSIONS_PREFIX: &str = "refs/caos/v4/sessions/";
pub const USERS_PREFIX: &str = "refs/caos/v4/users/";
const USER_SESSIONS: &str = "/sessions/";
/// Where sessions recorded before the rename keep their refs: the same keys,
/// under the names a session had when caos called it a conversation. Nothing
/// writes them any more; a reader that finds no v4 ref falls back to them
/// ([`legacy_ref`]), and the first write after that creates the v4 ref.
pub const LEGACY_SESSIONS_PREFIX: &str = "refs/caos/v3/conversations/";
pub const LEGACY_USERS_PREFIX: &str = "refs/caos/v3/users/";
const LEGACY_USER_SESSIONS: &str = "/conversations/";
pub const HEAD_SUFFIX: &str = "/head";
pub const MAX_SESSION_ID_BYTES: usize = 124;
pub const MAX_USER_ID_BYTES: usize = 126;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    Active,
    Archived,
}

pub fn validate_session_id(id: &str) -> Result<(), String> {
    validate_id(id, MAX_SESSION_ID_BYTES, "session")
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

pub fn head_ref(id: &str) -> Result<String, String> {
    validate_session_id(id)?;
    Ok(format!("{SESSIONS_PREFIX}{}{HEAD_SUFFIX}", key_of(id)))
}

pub fn active_membership_ref(user: &str, id: &str) -> Result<String, String> {
    membership_ref(user, Membership::Active, id)
}

pub fn archived_membership_ref(user: &str, id: &str) -> Result<String, String> {
    membership_ref(user, Membership::Archived, id)
}

pub fn parse_head_ref(refname: &str) -> Result<String, String> {
    let key = refname
        .strip_prefix(SESSIONS_PREFIX)
        .or_else(|| refname.strip_prefix(LEGACY_SESSIONS_PREFIX))
        .and_then(|rest| rest.strip_suffix(HEAD_SUFFIX))
        .ok_or_else(|| format!("invalid session head ref {refname:?}"))?;
    if key.contains('/') {
        return Err(format!("invalid session head ref {refname:?}"));
    }
    let id = id_of_key(key).map_err(|_| format!("invalid session head ref {refname:?}"))?;
    validate_session_id(&id).map_err(|_| format!("invalid session head ref {refname:?}"))?;
    Ok(id)
}

pub fn parse_membership_ref(refname: &str) -> Result<(String, Membership, String), String> {
    let (user_key, rest) = refname
        .strip_prefix(USERS_PREFIX)
        .and_then(|rest| rest.split_once(USER_SESSIONS))
        .or_else(|| {
            refname
                .strip_prefix(LEGACY_USERS_PREFIX)
                .and_then(|rest| rest.split_once(LEGACY_USER_SESSIONS))
        })
        .ok_or_else(|| format!("invalid session membership ref {refname:?}"))?;
    let (membership, session_key) = if let Some(key) = rest.strip_prefix("active/") {
        (Membership::Active, key)
    } else if let Some(key) = rest.strip_prefix("archived/") {
        (Membership::Archived, key)
    } else {
        return Err(format!("invalid session membership ref {refname:?}"));
    };
    if user_key.contains('/') || session_key.contains('/') {
        return Err(format!("invalid session membership ref {refname:?}"));
    }
    let user =
        id_of_key(user_key).map_err(|_| format!("invalid session membership ref {refname:?}"))?;
    let session = id_of_key(session_key)
        .map_err(|_| format!("invalid session membership ref {refname:?}"))?;
    validate_user_id(&user)
        .and_then(|()| validate_session_id(&session))
        .map_err(|_| format!("invalid session membership ref {refname:?}"))?;
    Ok((user, membership, session))
}

fn validate_id(id: &str, maximum: usize, kind: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > maximum || id.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(format!("invalid {kind} id {id:?}"));
    }
    Ok(())
}

fn membership_ref(user: &str, membership: Membership, id: &str) -> Result<String, String> {
    validate_user_id(user)?;
    validate_session_id(id)?;
    let kind = match membership {
        Membership::Active => "active",
        Membership::Archived => "archived",
    };
    Ok(format!(
        "{USERS_PREFIX}{}{USER_SESSIONS}{kind}/{}",
        key_of(user),
        key_of(id)
    ))
}

/// The pre-rename name of a v4 head or membership ref, which a reader falls
/// back to when the v4 ref does not exist: `refs/caos/v4/sessions/<k>/head` ->
/// `refs/caos/v3/conversations/<k>/head`, and
/// `refs/caos/v4/users/<u>/sessions/<m>/<k>` ->
/// `refs/caos/v3/users/<u>/conversations/<m>/<k>`. `None` for any other name.
pub fn legacy_ref(refname: &str) -> Option<String> {
    if let Some(rest) = refname.strip_prefix(SESSIONS_PREFIX) {
        return Some(format!("{LEGACY_SESSIONS_PREFIX}{rest}"));
    }
    let (user_key, rest) = refname.strip_prefix(USERS_PREFIX)?.split_once(USER_SESSIONS)?;
    Some(format!("{LEGACY_USERS_PREFIX}{user_key}{LEGACY_USER_SESSIONS}{rest}"))
}

/// The refs a session's head may be under, newest naming first: where to look
/// when reading it directly rather than through a store that falls back.
pub fn head_refs(id: &str) -> Result<[String; 2], String> {
    let current = head_ref(id)?;
    let legacy = legacy_ref(&current).expect("a head ref has a legacy name");
    Ok([current, legacy])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_refs_round_trip() {
        let refname = head_ref("talk-1").unwrap();
        assert_eq!(refname, "refs/caos/v4/sessions/74616c6b2d31/head");
        assert_eq!(parse_head_ref(&refname).unwrap(), "talk-1");
    }

    #[test]
    fn legacy_refs_name_the_same_session() {
        let head = head_ref("talk-1").unwrap();
        let legacy = legacy_ref(&head).unwrap();
        assert_eq!(legacy, "refs/caos/v3/conversations/74616c6b2d31/head");
        assert_eq!(parse_head_ref(&legacy).unwrap(), "talk-1");
        assert_eq!(head_refs("talk-1").unwrap(), [head, legacy]);

        let active = active_membership_ref("user@example", "talk-1").unwrap();
        let legacy = legacy_ref(&active).unwrap();
        assert_eq!(
            legacy,
            "refs/caos/v3/users/75736572406578616d706c65/conversations/active/74616c6b2d31"
        );
        assert_eq!(parse_membership_ref(&legacy), parse_membership_ref(&active));

        assert_eq!(legacy_ref("refs/heads/main"), None);
        assert_eq!(legacy_ref(&legacy), None);
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
            assert!(validate_session_id(id).is_err());
            assert!(validate_user_id(id).is_err());
        }
        assert!(validate_session_id(&"a".repeat(MAX_SESSION_ID_BYTES + 1)).is_err());
        assert!(validate_user_id(&"a".repeat(MAX_USER_ID_BYTES + 1)).is_err());
        for key in ["0", "AA", "gg", "ff"] {
            assert!(id_of_key(key).is_err(), "accepted {key:?}");
        }
    }
}
