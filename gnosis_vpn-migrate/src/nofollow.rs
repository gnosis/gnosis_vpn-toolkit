//! Root writes into a tree the user controls: every step goes through a held dir handle and never follows a symlink.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};

use crate::user::{self, Account};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The home itself comes from passwd, which the user cannot change, so its own path may be a symlink.
pub fn open_home(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::openat(CWD, path, flags, Mode::empty())?)
}

/// `None` when any component is missing; nothing is created.
pub fn find_dir(root: &OwnedFd, rel: &Path) -> io::Result<Option<OwnedFd>> {
    let mut dir = root.try_clone()?;
    for name in names(rel) {
        dir = match open_dir(&dir, name) {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
    }
    Ok(Some(dir))
}

/// Opens `rel` below `root`, creating missing components with `mode`, owned by the account.
pub fn ensure_dir(root: &OwnedFd, rel: &Path, mode: Mode, account: &Account) -> io::Result<OwnedFd> {
    let mut dir = root.try_clone()?;
    for name in names(rel) {
        dir = match open_dir(&dir, name) {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let child = create_dir(&dir, name, mode)?;
                user::chown(&child, account)?;
                child
            }
            Err(e) => return Err(e),
        };
    }
    Ok(dir)
}

/// Fails if `name` exists; the new dir stays owned by the caller (root) until it is chowned.
pub fn create_dir(parent: &OwnedFd, name: impl AsRef<OsStr>, mode: Mode) -> io::Result<OwnedFd> {
    let name = name.as_ref();
    rustix::fs::mkdirat(parent, name, mode)?;
    let dir = open_dir(parent, name)?;
    // mkdirat is subject to the umask.
    rustix::fs::fchmod(&dir, mode)?;
    Ok(dir)
}

pub fn open_dir(parent: &OwnedFd, name: impl AsRef<OsStr>) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(parent, name.as_ref(), DIR_FLAGS, Mode::empty())?)
}

/// Fails if `name` exists, including as a dangling symlink.
pub fn create_file(dir: &OwnedFd, name: impl AsRef<OsStr>, mode: Mode) -> io::Result<File> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(dir, name.as_ref(), flags, mode)?;
    rustix::fs::fchmod(&fd, mode)?;
    Ok(File::from(fd))
}

/// Read-only open of a path whose last component must not be a symlink.
pub fn open_file(path: &Path) -> io::Result<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(File::from(rustix::fs::openat(CWD, path, flags, Mode::empty())?))
}

pub fn exists(dir: &OwnedFd, name: impl AsRef<OsStr>) -> io::Result<bool> {
    match rustix::fs::statat(dir, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn rename(dir: &OwnedFd, from: impl AsRef<OsStr>, to: impl AsRef<OsStr>) -> io::Result<()> {
    Ok(rustix::fs::renameat(dir, from.as_ref(), dir, to.as_ref())?)
}

pub fn sync(fd: &OwnedFd) -> io::Result<()> {
    Ok(rustix::fs::fsync(fd)?)
}

/// Removes `name` and everything below it without following any symlink inside.
pub fn remove_tree(parent: &OwnedFd, name: impl AsRef<OsStr>) -> io::Result<()> {
    let name = name.as_ref();
    let dir = open_dir(parent, name)?;
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let child = OsStr::from_bytes(entry.file_name().to_bytes());
        if child == "." || child == ".." {
            continue;
        }
        if is_dir(&dir, child, entry.file_type())? {
            remove_tree(&dir, child)?;
        } else {
            rustix::fs::unlinkat(&dir, child, AtFlags::empty())?;
        }
    }
    Ok(rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?)
}

// Some filesystems report no type in the dir entry.
fn is_dir(dir: &OwnedFd, name: &OsStr, reported: FileType) -> io::Result<bool> {
    if reported != FileType::Unknown {
        return Ok(reported == FileType::Directory);
    }
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Directory)
}

fn names(rel: &Path) -> impl Iterator<Item = &OsStr> {
    rel.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}
