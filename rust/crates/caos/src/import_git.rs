//! Worker-side remote import client. Credentials only travel in a header.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut source = None;
    let mut revision = None;
    let mut invocation = None;
    let mut token_file = None;
    let mut json = false;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--invocation=") {
            if invocation.replace(value).is_some() {
                return Err("duplicate invocation".into());
            }
        } else if let Some(value) = arg.strip_prefix("--github-token-file=") {
            if token_file.replace(value).is_some() {
                return Err("duplicate token file".into());
            }
        } else if arg == "--json" && !json {
            json = true;
        } else if arg.starts_with('-') {
            return Err("unknown import-git option".into());
        } else if source.is_none() {
            source = Some(arg.as_str());
        } else if revision.is_none() {
            revision = Some(arg.as_str());
        } else {
            return Err("too many import-git arguments".into());
        }
    }
    let source = source.ok_or("import-git requires an HTTPS repository")?;
    git_locator::import::remote(source)?;
    git_locator::import::revision(revision)?;
    let generated;
    let invocation = match invocation {
        Some(value) => value,
        None => {
            use std::io::Read;
            let mut bytes = [0u8; 32];
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut bytes))
                .map_err(|_| "creating import identity")?;
            generated = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            &generated
        }
    };
    let scope_path = "/cas/args/secret-hash";
    let secret_scope = if std::path::Path::new(scope_path).exists() {
        crate::get(&crate::HttpTransport::from_env()?, scope_path, None)?;
        std::fs::read_to_string(scope_path).map_err(|_| "reading secret scope")?
    } else {
        String::new()
    };
    let body = serde_json::json!({"source":source,"revision":revision,"invocation":invocation,"secret_scope":secret_scope.trim()});
    let server = std::env::var(crate::SERVER_ENV).map_err(|_| "CAOS_SERVER_URL not set")?;
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if let Some(path) = token_file {
        let token = std::fs::read_to_string(path).map_err(|_| "reading Git token file")?;
        let token = token.trim_end_matches(['\r', '\n']);
        if token.is_empty() || token.len() > 8192 || !token.bytes().all(|b| b > b' ' && b < 127) {
            return Err("invalid Git token file".into());
        }
        headers.push((git_locator::import::TOKEN_HEADER, token.to_string()));
    }
    let body = body.to_string();
    let response = crate::server_request(
        &server,
        &crate::ServerRequest {
            method: "POST",
            path: "/git/import",
            headers: &headers,
            body: Some(body.as_bytes()),
            timeout_secs: Some(660),
        },
    )
    .map_err(|_| "Git import request failed; retry with the same invocation identity")?;
    if response.status != 200 {
        // The endpoint deliberately returns credential-free errors. Avoid
        // reflecting arbitrary proxy responses into a conversation nevertheless.
        return Err(format!(
            "Git import failed (HTTP {}); check repository, revision and credentials",
            response.status
        ));
    }
    let result: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid import response")?;
    let commit = result["commit"]
        .as_str()
        .filter(|c| git_locator::import::commit(c))
        .ok_or("import response lacks commit")?;
    if json {
        println!("{result}");
    } else {
        println!("{commit}");
    }
    Ok(())
}
