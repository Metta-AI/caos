//! The deliberately narrow remote-import surface. Existing locators are unchanged.
pub const TOKEN_HEADER: &str = "X-Caos-Git-Token";

pub fn commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn remote(source: &str) -> Result<(&str, &str), String> {
    let invalid = || {
        "import requires an HTTPS repository URL without credentials, query or fragment".to_string()
    };
    let rest = source.strip_prefix("https://").ok_or_else(invalid)?;
    let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
    if host.is_empty()
        || path.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-:".contains(&c))
        || host.starts_with('.')
        || host.starts_with('-')
        || source
            .bytes()
            .any(|c| c <= b' ' || c >= 127 || b"@?#%\\".contains(&c))
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid());
    }
    if let Some((name, port)) = host.split_once(':') {
        if name.is_empty() || port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
            return Err(invalid());
        }
    }
    Ok((host, path))
}

pub fn revision(value: Option<&str>) -> Result<String, String> {
    let Some(value) = value else {
        return Ok("HEAD".into());
    };
    if commit(value) {
        return Ok(value.to_ascii_lowercase());
    }
    if value.is_empty()
        || value == "@"
        || value.starts_with('-')
        || value.ends_with('.')
        || value.contains("..")
        || value.contains("@{")
        || value
            .bytes()
            .any(|b| b <= b' ' || b == 127 || b"~^:?*[\\".contains(&b))
        || value
            .split('/')
            .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
    {
        return Err("invalid remote revision".into());
    }
    Ok(if value == "HEAD" || value.starts_with("refs/") {
        value.into()
    } else {
        format!("refs/heads/{value}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_remote_import_surface() {
        for url in [
            "https://github.com/o/r.git",
            "https://localhost:5003/repo.git",
        ] {
            assert!(remote(url).is_ok());
        }
        for url in [
            "file:///tmp/x",
            "/tmp/x",
            "ext::x",
            "http://host/x",
            "https://token@host/x",
            "https://host/x?token=y",
            "https://host/x#x",
            "https://host/%2fetc",
            "https://host/../x",
            "https://host:bad/x",
            "https://host/x\n",
        ] {
            assert!(remote(url).is_err(), "{url}");
        }
        assert_eq!(revision(None).unwrap(), "HEAD");
        assert_eq!(revision(Some("main")).unwrap(), "refs/heads/main");
        assert_eq!(revision(Some("refs/tags/v1")).unwrap(), "refs/tags/v1");
        assert_eq!(revision(Some(&"A".repeat(40))).unwrap(), "a".repeat(40));
        for value in [
            "",
            "--upload-pack=x",
            "main:other",
            "a..b",
            "a.lock",
            "a\\b",
            "a*",
            "@{x}",
        ] {
            assert!(revision(Some(value)).is_err());
        }
    }
}
