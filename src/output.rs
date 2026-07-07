//! Standard-output protocol.
//!
//! Every protocol event is written to stdout as a single line of JSON
//! (newline-delimited JSON / NDJSON). `gnosis_vpn-app` spawns the toolkit and
//! reads stdout line by line, parsing each line as one JSON value. Human logs
//! and diagnostics go to stderr (see [`crate::logging`]), never here.

use std::io::Write;

use serde::Serialize;

/// Serialize `value` and write it to stdout as one JSON line, flushing so the
/// consuming process sees each event promptly. Serialization/IO failures are
/// logged to stderr and otherwise ignored (a closed stdout means the consumer
/// went away — there is nothing useful to do).
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
