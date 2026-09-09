use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Option<Instant>> = OnceLock::new();

pub fn start() {
    let _ = START.set(std::env::var_os("CAOS_STEP_TIMING").map(|_| Instant::now()));
}

pub fn phase(name: &str) {
    let Some(start) = START
        .get_or_init(|| std::env::var_os("CAOS_STEP_TIMING").map(|_| Instant::now()))
        .as_ref()
    else {
        return;
    };
    eprintln!("llm-step timing: {}ms {name}", start.elapsed().as_millis());
}
