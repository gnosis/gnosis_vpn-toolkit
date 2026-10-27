//! Logs go to stderr (stdout is the protocol), level via `RUST_LOG`, optionally teed into a log file.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

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

/// The legacy release gave the log dir to the service account, so no path component is ever followed and a FIFO never blocks (O_NONBLOCK).
fn open_append(path: &Path) -> io::Result<File> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other("not a file path"));
    };
    let dir = ensure_dir_nofollow(parent)?;
    let flags =
        OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(&dir, name, flags, Mode::from_bits_truncate(0o644))?;
    let stat = rustix::fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::other("not a regular file"));
    }
    if stat.st_nlink != 1 {
        return Err(io::Error::other("has more than one hard link"));
    }
    Ok(File::from(fd))
}

/// `create_dir_all` resolves symlinks in every component; this walks them one no-follow handle at a time instead.
fn ensure_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let start = if path.is_absolute() { "/" } else { "." };
    let mut dir = rustix::fs::openat(CWD, start, flags, Mode::empty())?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        dir = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(child) => child,
            Err(rustix::io::Errno::NOENT) => {
                rustix::fs::mkdirat(&dir, name, Mode::from_bits_truncate(0o755))?;
                rustix::fs::openat(&dir, name, flags, Mode::empty())?
            }
            Err(e) => return Err(e.into()),
        };
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Canonical so the no-follow walk is not tripped by the `/var -> /private/var` link under macOS temp dirs.
    fn temp_root(tag: &str) -> std::path::PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("toolkit-logging-{tag}-{}", std::process::id()))
    }

    #[test]
    fn open_append_creates_the_directory_and_appends() {
        let dir = temp_root("append");
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
        let dir = temp_root("file");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        fs::write(&blocker, "x").unwrap();
        assert!(open_append(&blocker.join("migrate.log")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_append_refuses_a_planted_symlink_hard_link_or_fifo() {
        let dir = temp_root("planted");
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

    #[test]
    fn open_append_refuses_a_symlinked_parent() {
        let dir = temp_root("parent");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("installer")).unwrap();
        assert!(open_append(&dir.join("installer").join("migrate.log")).is_err());
        assert!(!elsewhere.join("migrate.log").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
