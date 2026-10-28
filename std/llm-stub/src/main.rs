//! llm-stub: a scripted stand-in for the LLM API, for worker tests.
//!
//! `llm-stub <addr> <dir>` serves `POST /v1/messages` from `<dir>`: the i-th
//! request (1-based) is answered with the contents of `<dir>/response-<i>.json`
//! (HTTP 200, application/json), and its body is recorded verbatim at
//! `<dir>/request-<i>.json` so the test can assert exactly what the worker
//! sent — e.g. that a later round replayed an earlier round's assistant blocks
//! byte-for-byte. An unscripted request (no response file) gets a 500, which
//! errors the run loudly.
//!
//! Requests are handled sequentially — the step loop is sequential by design,
//! so ordering is deterministic.
//!
//! A test whose requests arrive in an order it cannot know (a parent and a
//! child agent sharing one stub) scripts REPLIES BY REQUEST instead. When
//! `<dir>/response-<i>.json` is absent, the first rule under `<dir>/rules/`
//! that matches the request body answers it. A rule is a directory
//! `rules/<name>/` holding `response.json`, an optional `contains` file and an
//! optional `lacks` file, one needle per line (blank lines are ignored). It
//! matches when every `contains` needle occurs in the raw request body and no
//! `lacks` needle does. Rules are tried in name order, so number them and put
//! the most specific first. The test writes them all before it starts the
//! turn, so it never has to watch for a request.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("llm-stub: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (addr, dir) = match (args.get(1), args.get(2)) {
        (Some(addr), Some(dir)) => (addr.clone(), dir.clone()),
        _ => return Err("usage: llm-stub <addr> <script-dir>".to_string()),
    };
    let server = tiny_http::Server::http(&addr).map_err(|e| format!("binding {addr}: {e}"))?;
    eprintln!("llm-stub listening on {addr}, script dir {dir}");

    let mut round = 0u32;
    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        if request.method() != &tiny_http::Method::Post || !url.starts_with("/v1/messages") {
            let _ = request.respond(
                tiny_http::Response::from_string("llm-stub: not found").with_status_code(404),
            );
            continue;
        }
        round += 1;
        let mut body = Vec::new();
        request
            .as_reader()
            .read_to_end(&mut body)
            .map_err(|e| format!("reading request {round}: {e}"))?;
        std::fs::write(format!("{dir}/request-{round}.json"), &body)
            .map_err(|e| format!("recording request {round}: {e}"))?;

        let scripted = std::fs::read(format!("{dir}/response-{round}.json"))
            .ok()
            .or_else(|| {
                let (name, scripted) = match_rule(&dir, &body)?;
                eprintln!("llm-stub: round {round}: rule {name}");
                Some(scripted)
            });
        let response = match scripted {
            Some(scripted) => {
                eprintln!("llm-stub: round {round}: {} bytes", scripted.len());
                tiny_http::Response::from_data(scripted).with_header(
                    tiny_http::Header::from_bytes("content-type", "application/json")
                        .expect("static header"),
                )
            }
            None => {
                eprintln!("llm-stub: round {round}: UNSCRIPTED");
                tiny_http::Response::from_data(
                    format!("llm-stub: no response-{round}.json scripted and no rule matched")
                        .into_bytes(),
                )
                .with_status_code(500)
            }
        };
        request
            .respond(response)
            .map_err(|e| format!("responding to request {round}: {e}"))?;
    }
    Ok(())
}

/// The first rule under `<dir>/rules/` that matches the raw request body, with
/// its name and its `response.json`. See the module doc for the rule format.
fn match_rule(dir: &str, body: &[u8]) -> Option<(String, Vec<u8>)> {
    let body = String::from_utf8_lossy(body);
    let mut names: Vec<String> = std::fs::read_dir(format!("{dir}/rules"))
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().find_map(|name| {
        let rule = format!("{dir}/rules/{name}");
        let needles = |file: &str| -> Vec<String> {
            std::fs::read_to_string(format!("{rule}/{file}"))
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let matches = needles("contains").iter().all(|n| body.contains(n.as_str()))
            && !needles("lacks").iter().any(|n| body.contains(n.as_str()));
        if !matches {
            return None;
        }
        let response = std::fs::read(format!("{rule}/response.json")).ok()?;
        Some((name, response))
    })
}
