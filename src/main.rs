//! VeloX entry point. All real work happens in [`velox::app`].

fn main() {
    // Captured unconditionally (a single cheap clock read) so that, if
    // performance metrics turn out to be enabled once `Config` is loaded,
    // "process start" is as close to the real start as possible. See
    // `Config::perf_metrics` / `docs/architecture.md`.
    let process_start = std::time::Instant::now();
    let config = velox::config::Config::from_env_and_args(std::env::args().skip(1));
    if let Err(err) = velox::app::run(config, process_start) {
        eprintln!("velox: fatal error: {err}");
        std::process::exit(1);
    }
}
