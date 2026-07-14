//! llm-stub: a scripted stand-in for the LLM API, for the llm-step tests.
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

        let response = match std::fs::read(format!("{dir}/response-{round}.json")) {
            Ok(scripted) => {
                eprintln!("llm-stub: round {round}: {} bytes", scripted.len());
                tiny_http::Response::from_data(scripted).with_header(
                    tiny_http::Header::from_bytes("content-type", "application/json")
                        .expect("static header"),
                )
            }
            Err(_) => {
                eprintln!("llm-stub: round {round}: UNSCRIPTED");
                tiny_http::Response::from_data(
                    format!("llm-stub: no response-{round}.json scripted").into_bytes(),
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
