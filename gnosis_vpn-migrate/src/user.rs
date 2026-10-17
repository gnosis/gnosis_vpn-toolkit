//! The installing user: passwd lookup and ownership of what the migration creates.

use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use rustix::fs::{Gid, Uid};
use uzers::os::unix::UserExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Refuses root (the client never runs a worker as uid 0) and a home that is missing or is the legacy dir itself.
pub fn resolve(name: &str, legacy_home: &Path) -> Result<Account, String> {
    if name == "root" {
        return Err("refusing to migrate into root's home: the client never runs a worker as uid 0".to_string());
    }
    let Some(user) = uzers::get_user_by_name(name) else {
        return Err(format!("unknown user {name}"));
    };
    let home = user.home_dir().to_path_buf();
    if !home.is_dir() {
        return Err(format!("home of {name} is not a directory: {}", home.display()));
    }
    let same_dir = same_path(&home, legacy_home);
    if same_dir {
        return Err(format!(
            "home of {name} is the legacy data dir {}; pass the account the client will run as",
            home.display()
        ));
    }
    Ok(Account {
        name: name.to_string(),
        uid: user.uid(),
        gid: user.primary_group_id(),
        home,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Through the open handle, so a symlink swapped in after opening cannot redirect it.
pub fn chown(fd: impl AsFd, account: &Account) -> io::Result<()> {
    let owner = Some(Uid::from_raw(account.uid));
    let group = Some(Gid::from_raw(account.gid));
    rustix::fs::fchown(fd, owner, group)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_refused_before_any_lookup() {
        let err = resolve("root", Path::new("/var/lib/gnosisvpn")).unwrap_err();
        assert!(err.contains("uid 0"), "{err}");
    }

    #[test]
    fn unknown_user_is_an_error() {
        let err = resolve("no-such-user-gno960", Path::new("/var/lib/gnosisvpn")).unwrap_err();
        assert!(err.contains("unknown user"), "{err}");
    }

    #[test]
    fn the_current_user_resolves_to_its_passwd_entry() {
        // Under `just test-root` the current user is root, which resolve refuses by design.
        if is_root() {
            return;
        }
        let me = uzers::get_user_by_uid(uzers::get_effective_uid()).unwrap();
        let name = me.name().to_str().unwrap();
        let account = resolve(name, Path::new("/var/lib/gnosisvpn")).unwrap();
        assert_eq!(account.uid, me.uid());
        assert_eq!(account.gid, me.primary_group_id());
        assert_eq!(account.home, me.home_dir());
    }

    #[test]
    fn a_home_equal_to_the_legacy_dir_is_refused() {
        if is_root() {
            return;
        }
        let me = uzers::get_user_by_uid(uzers::get_effective_uid()).unwrap();
        let name = me.name().to_str().unwrap();
        let err = resolve(name, me.home_dir()).unwrap_err();
        assert!(err.contains("legacy data dir"), "{err}");
    }
}
