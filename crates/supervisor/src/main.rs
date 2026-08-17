//! `arcade-supervisor` binary: a thin shell over the [`supervisor`]
//! library. All behavior lives in the library so tests can drive it
//! without processes; this file only wires up logging, reads the config,
//! and enters the forever loop.

use supervisor::config::Config;
use supervisor::main_loop;

#[tokio::main]
async fn main() {
    // Logs go to stderr (the journal under systemd/cage). RUST_LOG
    // overrides the default `info` level.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::from_env();
    tracing::info!(?cfg, "arcade-supervisor starting");
    main_loop::run_forever(cfg).await;
}
