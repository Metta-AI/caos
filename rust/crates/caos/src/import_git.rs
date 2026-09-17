//! Worker-side client for importing an exact Git commit.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut token_file = None;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--github-token-file=") {
            if token_file.replace(value).is_some() {
                return Err("duplicate token file".into());
            }
        } else if arg.starts_with('-') {
            return Err("unknown import-git option".into());
        } else {
            positional.push(arg.as_str());
        }
    }
    let [source, commit] = positional.as_slice() else {
        return Err("import-git requires an HTTPS repository and full commit hash".into());
    };
    git_locator::import::remote(source)?;
    if !git_locator::import::commit(commit) {
        return Err("import-git requires a full commit hash".into());
    }
    let commit = commit.to_ascii_lowercase();
    let body = serde_json::json!({"source":source, "commit":commit}).to_string();
    let response = send(source, "/git/import", &body, token_file)?;
    if response.status != 200 {
        return Err(format!(
            "Git import failed (HTTP {}); check repository, commit and credentials",
            response.status
        ));
    }
    let result: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid import response")?;
    if result["commit"].as_str() != Some(&commit) {
        return Err("import response does not match requested commit".into());
    }
    println!("{commit}");
    Ok(())
}

pub(crate) fn send(
    source: &str,
    endpoint: &str,
    body: &str,
    token_file: Option<&str>,
) -> Result<crate::ServerResponse, String> {
    let server = std::env::var(crate::SERVER_ENV).map_err(|_| "CAOS_SERVER_URL not set")?;
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if let Some(path) = token_file {
        let token = std::fs::read_to_string(path).map_err(|_| "reading Git token file")?;
        let token = token.trim_end_matches(['\r', '\n']);
        git_locator::import::git(source, Some(token))?;
        headers.push((git_locator::import::TOKEN_HEADER, token.to_string()));
    }
    let response = crate::server_request(
        &server,
        &crate::ServerRequest {
            method: "POST",
            path: endpoint,
            headers: &headers,
            body: Some(body.as_bytes()),
            timeout_secs: Some(960),
        },
    )
    .map_err(|_| "Git request failed; retain the pinned commit and lease when reconciling")?;
    Ok(response)
}
