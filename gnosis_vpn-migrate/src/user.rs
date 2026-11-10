//! The installing user: passwd lookup and ownership of what the migration creates.

use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Gid, Uid};
use uzers::os::unix::UserExt;

use crate::nofollow;
use crate::relocate::Problem;

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
    let Some(user) = uzers::get_user_by_name(name) else {
        return Err(format!("unknown user {name}"));
    };
    // By uid, not name: aliases like `toor` share uid 0.
    if user.uid() == 0 {
        return Err(format!(
            "refusing to migrate into {name}'s home: the client never runs a worker as uid 0"
        ));
    }
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

/// Where local accounts are listed by name, so each is found whoever owns its home and whatever that home is called.
#[cfg(target_os = "linux")]
const LOCAL_ACCOUNTS: &str = "/etc/passwd";
#[cfg(target_os = "macos")]
const LOCAL_ACCOUNTS: &str = "/var/db/dslocal/nodes/Default/users";

/// Every non-root account whose passwd home is a dir right under one of `roots`, judged by that home and not by who owns it.
/// Candidates are the owner and the namesake of each dir there plus every local account; the legacy home is left out.
/// A root, entry or account list that cannot be read is returned as a problem: its user would otherwise be skipped silently.
pub fn local_accounts(roots: &[PathBuf], legacy_home: &Path) -> (Vec<Account>, Vec<Problem>) {
    let mut candidates = Vec::new();
    let mut problems = Vec::new();
    for root in roots {
        let dir = match nofollow::open_home(root) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(problem(root, format!("cannot open home root: {e}")));
                continue;
            }
        };
        let names = match nofollow::entries(&dir) {
            Ok(names) => names,
            Err(e) => {
                problems.push(problem(root, format!("cannot list home dirs: {e}")));
                continue;
            }
        };
        for name in names {
            // Follows a top-level link (`/home/alice -> /mnt/alice` is a valid home); nothing below a home is followed.
            match rustix::fs::statat(&dir, &name, AtFlags::empty()) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
                    candidates.extend(uzers::get_user_by_uid(stat.st_uid));
                }
                Ok(_) | Err(rustix::io::Errno::NOENT) => continue,
                Err(e) => {
                    problems.push(problem(&root.join(&name), format!("cannot inspect: {e}")));
                    continue;
                }
            }
            // A root-owned home still belongs to the account it is named after.
            if let Some(name) = name.to_str() {
                candidates.extend(uzers::get_user_by_name(name));
            }
        }
    }
    #[cfg(target_os = "linux")]
    let local = passwd_names(Path::new(LOCAL_ACCOUNTS));
    #[cfg(target_os = "macos")]
    let local = dslocal_names(Path::new(LOCAL_ACCOUNTS));
    match local {
        Ok(names) => candidates.extend(names.iter().filter_map(uzers::get_user_by_name)),
        Err(e) => problems.push(problem(Path::new(LOCAL_ACCOUNTS), format!("cannot read: {e}"))),
    }
    let accounts = accounts_among(candidates, roots, legacy_home, &mut problems);
    (accounts, problems)
}

/// Each candidate found more than once is kept once, by uid and home: accounts sharing a uid may each have their own home.
fn accounts_among(
    candidates: Vec<uzers::User>,
    roots: &[PathBuf],
    legacy_home: &Path,
    problems: &mut Vec<Problem>,
) -> Vec<Account> {
    let mut accounts: Vec<Account> = Vec::new();
    for user in candidates {
        if accounts
            .iter()
            .any(|a| a.uid == user.uid() && a.home == user.home_dir())
        {
            continue;
        }
        match account_under(&user, roots, legacy_home) {
            Some(Ok(account)) => accounts.push(account),
            Some(Err(e)) => problems.push(problem(user.home_dir(), e)),
            None => {}
        }
    }
    accounts
}

/// `Some` when `user` is not root and its passwd home is an existing dir right under one of `roots`, other than the legacy home.
fn account_under(user: &uzers::User, roots: &[PathBuf], legacy_home: &Path) -> Option<Result<Account, String>> {
    let home = user.home_dir();
    let under_root = home
        .parent()
        .is_some_and(|parent| roots.iter().any(|root| root.as_path() == parent));
    let is_user_home = under_root && home.is_dir();
    let is_root_account = user.uid() == 0;
    let is_legacy_home = same_path(home, legacy_home);
    if !is_user_home || is_root_account || is_legacy_home {
        return None;
    }
    let Some(name) = user.name().to_str() else {
        return Some(Err("account name is not UTF-8".to_string()));
    };
    Some(Ok(Account {
        name: name.to_string(),
        uid: user.uid(),
        gid: user.primary_group_id(),
        home: home.to_path_buf(),
    }))
}

/// The account names in a passwd file; comments and blank lines are skipped.
#[cfg(any(target_os = "linux", test))]
fn passwd_names(path: &Path) -> io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// The account names of macOS's local directory node: one `<name>.plist` record each.
#[cfg(any(target_os = "macos", test))]
fn dslocal_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let file_name = entry?.file_name();
        if let Some(name) = file_name.to_str().and_then(|name| name.strip_suffix(".plist"))
            && !name.is_empty()
        {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn problem(path: &Path, error: String) -> Problem {
    Problem {
        path: path.to_path_buf(),
        error,
    }
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
    fn root_is_refused() {
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

    #[test]
    fn the_current_user_is_found_under_its_home_root() {
        use std::os::unix::fs::MetadataExt;
        let me = uzers::get_user_by_uid(uzers::get_effective_uid()).unwrap();
        let home = me.home_dir().to_path_buf();
        // A build user's root-owned home (`/var/empty` on macOS) is found only via the account list, which needs root there.
        let owns_home = std::fs::metadata(&home).is_ok_and(|meta| meta.uid() == me.uid());
        let Some(root) = home.parent().filter(|_| !is_root() && owns_home) else {
            return;
        };
        let (accounts, _) = local_accounts(&[root.to_path_buf()], Path::new("/var/lib/gnosisvpn"));
        assert!(accounts.iter().any(|a| a.uid == me.uid() && a.home == home));
        let (accounts, _) = local_accounts(&[root.to_path_buf()], &home);
        assert!(
            !accounts.iter().any(|a| a.uid == me.uid()),
            "the legacy home is never an account"
        );
    }

    #[test]
    fn dirs_whose_owner_lives_elsewhere_links_and_missing_roots_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("someone")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("someone"), tmp.path().join("link")).unwrap();
        let roots = [tmp.path().to_path_buf(), tmp.path().join("missing")];
        let (accounts, problems) = local_accounts(&roots, Path::new("/var/lib/gnosisvpn"));
        assert!(accounts.is_empty());
        // An unprivileged macOS run cannot read the root-only account list; only problems under the temp dir count.
        assert!(!problems.iter().any(|p| p.path.starts_with(tmp.path())));
    }

    #[test]
    fn an_unreadable_home_root_is_a_problem_not_a_silent_skip() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads any dir, so only an unprivileged run can see the failure.
        if is_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("homes");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (accounts, problems) = local_accounts(std::slice::from_ref(&root), Path::new("/var/lib/gnosisvpn"));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(accounts.is_empty());
        let under_tmp: Vec<_> = problems.iter().filter(|p| p.path.starts_with(tmp.path())).collect();
        assert_eq!(under_tmp.len(), 1);
        assert_eq!(under_tmp[0].path, root);
    }

    fn user_with_home(uid: u32, name: &str, home: &Path) -> uzers::User {
        uzers::User::new(uid, name, uid).with_home_dir(home)
    }

    #[test]
    fn an_account_counts_by_its_passwd_home_whoever_owns_it_and_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        std::fs::create_dir_all(root.join("alice")).unwrap();
        std::fs::create_dir_all(tmp.path().join("mnt/bob")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("mnt/bob"), root.join("bob")).unwrap();
        let roots = [root.clone()];
        let legacy = Path::new("/var/lib/gnosisvpn");
        // Neither dir is owned by uid 4242 or 4343: only the passwd home decides.
        for (uid, name) in [(4242, "alice"), (4343, "bob")] {
            let account = account_under(&user_with_home(uid, name, &root.join(name)), &roots, legacy)
                .unwrap()
                .unwrap();
            assert_eq!((account.uid, account.home), (uid, root.join(name)));
        }
    }

    #[test]
    fn root_homes_elsewhere_missing_homes_and_the_legacy_home_are_no_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        std::fs::create_dir_all(root.join("alice")).unwrap();
        let roots = [root.clone()];
        let legacy = Path::new("/var/lib/gnosisvpn");
        assert!(account_under(&user_with_home(0, "toor", &root.join("alice")), &roots, legacy).is_none());
        assert!(account_under(&user_with_home(4242, "alice", tmp.path()), &roots, legacy).is_none());
        assert!(account_under(&user_with_home(4242, "alice", &root.join("gone")), &roots, legacy).is_none());
        assert!(
            account_under(
                &user_with_home(4242, "alice", &root.join("alice")),
                &roots,
                &root.join("alice")
            )
            .is_none()
        );
    }

    #[test]
    fn accounts_sharing_a_uid_each_count_with_their_own_home_and_repeats_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        for name in ["alice", "bob"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let alice = user_with_home(1000, "alice", &root.join("alice"));
        let bob = user_with_home(1000, "bob", &root.join("bob"));
        let candidates = vec![alice.clone(), bob, alice];
        let mut problems = Vec::new();
        let accounts = accounts_among(
            candidates,
            std::slice::from_ref(&root),
            Path::new("/var/lib/gnosisvpn"),
            &mut problems,
        );
        let homes: Vec<_> = accounts.iter().map(|a| a.home.clone()).collect();
        assert_eq!(homes, [root.join("alice"), root.join("bob")]);
        assert!(problems.is_empty());
    }

    #[test]
    fn dslocal_names_are_the_plist_records() {
        let tmp = tempfile::tempdir().unwrap();
        for file in ["alice.plist", "_www.plist", ".plist", "notes.txt"] {
            std::fs::write(tmp.path().join(file), "").unwrap();
        }
        let mut names = dslocal_names(tmp.path()).unwrap();
        names.sort();
        assert_eq!(names, ["_www", "alice"]);
    }

    #[test]
    fn passwd_names_skip_comments_and_blank_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let passwd = tmp.path().join("passwd");
        std::fs::write(
            &passwd,
            "root:x:0:0::/root:/bin/sh\n# comment\n\nalice:x:1000:1000::/home/alice:/bin/bash\n",
        )
        .unwrap();
        assert_eq!(passwd_names(&passwd).unwrap(), ["root", "alice"]);
    }
}
