//! Tracing setup.
//!
//! Logs go to **stderr** — stdout is reserved for the machine-readable NDJSON
//! protocol consumed by `gnosis_vpn-app`. Level is controlled by `RUST_LOG`
//! (default `info`).

use tracing_subscriber::EnvFilter;

pub fn setup() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
