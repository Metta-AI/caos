//! Validation for Git sources and publication destinations.

pub fn validate_source(source: &str) -> Result<git_locator::GitRef, String> {
    let parsed = git_locator::parse_git_ref(source)?;
    if parsed.rev.is_none() || parsed.dir.is_some() {
        return Err("source tree source must pin a whole Git commit (no path: or dir=)".into());
    }
    validate_repository(&parsed.fetch_url())?;
    Ok(parsed)
}

pub fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.is_empty()
        || repository.starts_with('-')
        || repository.chars().any(char::is_control)
    {
        return Err("invalid source tree repository".into());
    }
    if repository.split_once("://").is_some_and(|(scheme, rest)| {
        rest.split('/')
            .next()
            .and_then(|host| host.rsplit_once('@'))
            .is_some_and(|(user, _)| scheme != "ssh" || user.contains(':'))
    }) {
        return Err("repository URLs must not contain credentials".into());
    }
    Ok(())
}

pub fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || branch == "@"
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || b"~^:?*[\\".contains(&byte))
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(format!("invalid branch name {branch:?}"));
    }
    Ok(())
}

pub fn normalize_repository_identity(url: &str) -> Result<String, String> {
    let url = url.trim();
    let url = url.strip_prefix("file://").unwrap_or(url);
    if url.is_empty() {
        return Err("origin has an empty URL".to_string());
    }
    let ssh;
    let url = if let Some(rest) = url.strip_prefix("ssh://git@") {
        ssh = format!("https://{rest}");
        &ssh
    } else {
        url
    };
    let mut normalized = if let Some(scp) = url.strip_prefix("git@") {
        let (host, path) = scp
            .split_once(':')
            .ok_or_else(|| format!("invalid origin URL {url:?}"))?;
        if host.is_empty() || path.is_empty() {
            return Err(format!("invalid origin URL {url:?}"));
        }
        format!("https://{host}/{path}")
    } else {
        url.to_string()
    };
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if let Some(without_suffix) = normalized.strip_suffix(".git") {
        normalized = without_suffix.to_string();
    }
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if let Some((scheme, rest)) = normalized.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("invalid origin URL {url:?}"));
        }
        let authority = match authority.rsplit_once('@') {
            Some((user, host)) => format!("{user}@{}", host.to_ascii_lowercase()),
            None => authority.to_ascii_lowercase(),
        };
        normalized = if path.is_empty() {
            format!("{scheme}://{authority}")
        } else {
            format!("{scheme}://{authority}/{path}")
        };
    }
    if normalized.is_empty() {
        return Err(format!("invalid origin URL {url:?}"));
    }
    Ok(normalized)
}
