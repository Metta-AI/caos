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

/// Credentials stay in the child environment and are scoped to this HTTPS URL.
pub fn git(source: &str, token: Option<&str>) -> Result<std::process::Command, String> {
    use std::process::Command;
    let (host, path) = remote(source)?;
    if token
        .is_some_and(|t| t.is_empty() || t.len() > 8192 || !t.bytes().all(|b| b > b' ' && b < 127))
    {
        return Err("invalid Git token".into());
    }
    // The helper receives only this process's credential context. Refuse
    // redirects and all other protocols; never persist credentials or echo
    // remote-controlled stderr (it can contain credentials).
    const HELPER: &str = "!f() { test \"$1\" = get || exit 0; protocol= host= path=; while IFS='=' read -r k v; do case \"$k\" in protocol) protocol=$v;; host) host=$v;; path) path=$v;; esac; done; if test \"$protocol\" = https && test \"$host\" = \"$CAOS_IMPORT_HOST\" && test \"$path\" = \"$CAOS_IMPORT_PATH\"; then printf 'username=x-access-token\\npassword=%s\\n' \"$CAOS_IMPORT_TOKEN\"; fi; }; f";
    let mut cmd = Command::new("timeout");
    cmd.args(["--kill-after=5", "300", "git"]);
    // Whitelist inherited environment: in particular no Git traces, askpass,
    // config injection, URL rewrites or ambient credential helpers.
    cmd.env_clear();
    for name in ["PATH", "SSL_CERT_FILE", "GIT_SSL_CAINFO"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "credential.useHttpPath=true",
            "-c",
            "http.followRedirects=false",
            "-c",
            "http.extraHeader=",
            "-c",
            "fetch.writeCommitGraph=false",
        ]);
    if let Some(token) = token {
        cmd.env("CAOS_IMPORT_TOKEN", token)
            .env("CAOS_IMPORT_HOST", host)
            .env("CAOS_IMPORT_PATH", path)
            .args(["-c", &format!("credential.{}.helper={HELPER}", source)]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Ok(cmd)
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
        assert!(commit(&"A".repeat(40)));
        for value in ["main", "HEAD", "", "abc", &"a".repeat(64)] {
            assert!(!commit(value));
        }
    }
}
