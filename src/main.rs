//! VeloX entry point. All real work happens in [`velox::app`].

fn main() {
    let config = velox::config::Config::from_env_and_args(std::env::args().skip(1));
    if let Err(err) = velox::app::run(config) {
        eprintln!("velox: fatal error: {err}");
        std::process::exit(1);
    }
}
