//! stdout protocol: one JSON line per event (NDJSON); logs go to stderr, never here.

use std::io::Write;

use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Newline-delimited JSON on stdout — what machine consumers ask for.
    Json,
    /// Human-readable lines on stdout (the default).
    Plain,
}

/// Flushes per line so the consumer sees events promptly; a closed stdout means it went away, so IO errors are only logged.
pub fn emit<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(line) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = writeln!(lock, "{line}").and_then(|()| lock.flush()) {
                tracing::debug!(error = %e, "failed to write to stdout");
            }
        }
        Err(e) => tracing::error!(error = %e, "failed to serialize output event"),
    }
}
