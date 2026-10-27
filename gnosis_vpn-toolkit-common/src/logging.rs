//! Logs go to stderr (stdout is the protocol), level via `RUST_LOG`, optionally teed into a log file.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use rustix::fs::{CWD, FileType, Mode, OFlags};
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

/// The legacy release gave the log dir to the service account, so a planted symlink is never followed and a FIFO never blocks (O_NONBLOCK).
fn open_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let flags =
        OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(CWD, path, flags, Mode::from_bits_truncate(0o644))?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::other("not a regular file"));
    }
    if stat.st_nlink != 1 {
        return Err(io::Error::other("has more than one hard link"));
    }
    Ok(File::from(fd))
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

    #[test]
    fn open_append_refuses_a_planted_symlink_hard_link_or_fifo() {
        let dir = std::env::temp_dir().join(format!("toolkit-logging-planted-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, "untouched").unwrap();
        let link = dir.join("link.log");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(open_append(&link).is_err());
        let hard_link = dir.join("hard.log");
        fs::hard_link(&victim, &hard_link).unwrap();
        assert!(open_append(&hard_link).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");

        let fifo = dir.join("fifo.log");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(open_append(&fifo).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
