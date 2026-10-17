//! Logs go to stderr (stdout is the protocol), level via `RUST_LOG`, optionally teed into a log file.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};

/// A log file that cannot be opened degrades to stderr-only; failing the installer over a log file would lose diagnostics, not gain them.
pub fn setup(log_file: Option<&Path>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (file, open_error) = match log_file.map(open_append) {
        Some(Ok(file)) => (Some(file), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let writer = match file {
        Some(file) => BoxMakeWriter::new(std::io::stderr.and(file)),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        // stderr is captured by the app or the installer, never a terminal, and the file copy stays greppable
        .with_ansi(false)
        .init();
    // Bypasses RUST_LOG on purpose: a missing log must be visible regardless of the level.
    if let (Some(path), Some(e)) = (log_file, open_error) {
        eprintln!("log file {} not writable ({e}), logging to stderr only", path.display());
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_append_creates_the_directory_and_appends() {
        let dir = std::env::temp_dir().join(format!("toolkit-logging-{}", std::process::id()));
        let path = dir.join("nested").join("migrate.log");
        use std::io::Write;
        writeln!(open_append(&path).unwrap(), "one").unwrap();
        writeln!(open_append(&path).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_append_reports_an_unwritable_location() {
        // A file where a directory is needed makes the open fail without needing privileges.
        let dir = std::env::temp_dir().join(format!("toolkit-logging-file-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        fs::write(&blocker, "x").unwrap();
        assert!(open_append(&blocker.join("migrate.log")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
