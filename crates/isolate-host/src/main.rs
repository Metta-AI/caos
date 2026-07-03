fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("smoke") => isolate_host::smoke().map(|elapsed| {
            println!(
                "isolate-host smoke ok (instantiate: {}us)",
                elapsed.as_micros()
            );
        }),
        Some(other) => Err(format!(
            "unknown subcommand {other:?}; use `smoke` or no args"
        )),
        None => isolate_host::serve(),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("isolate-host: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
