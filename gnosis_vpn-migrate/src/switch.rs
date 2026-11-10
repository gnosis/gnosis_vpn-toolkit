//! `switch-network`: carries every user's identity from a retired network to the configured one.
//! The safe and PIX state belong to the old network's contracts, so they stay behind; the client finds or deploys a new safe.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::layout::{self, UserLayout};
use crate::migrate::{self, Failure, Move, Networks, Place, Planned, Stage};
use crate::nofollow;
use crate::relocate::Problem;
use crate::user::Account;

pub struct SwitchRequest<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub legacy_home: &'a Path,
    pub accounts: &'a [Account],
    pub toolkit_version: &'a str,
}

/// What a run did; one account's failure is listed and never stops the others.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub from: String,
    pub to: String,
    pub copied: Vec<Move>,
    /// Per-network dirs of users who already had an identity on `to`; theirs wins.
    pub kept: Vec<PathBuf>,
    pub safe_backups: Vec<Move>,
    pub moved_aside: Vec<Move>,
    pub failed: Vec<Problem>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty()
            && self.kept.is_empty()
            && self.safe_backups.is_empty()
            && self.moved_aside.is_empty()
            && self.failed.is_empty()
    }
}

pub fn switch_network(req: &SwitchRequest<'_>) -> Report {
    let mut report = Report {
        from: req.from.to_string(),
        to: req.to.to_string(),
        ..Report::default()
    };
    match back_up_legacy_safe(req.legacy_home, req.from) {
        Ok(Some(m)) => report.safe_backups.push(m),
        Ok(None) => {}
        Err(e) => report.failed.push(Problem {
            path: req.legacy_home.to_path_buf(),
            error: e.error,
        }),
    }
    match move_aside_legacy_pix(req.legacy_home, req.from) {
        Ok(Some(m)) => report.moved_aside.push(m),
        Ok(None) => {}
        Err(e) => report.failed.push(Problem {
            path: req.legacy_home.to_path_buf(),
            error: e.error,
        }),
    }
    let ts = migrate::timestamp();
    for account in req.accounts {
        if let Err(e) = switch_account(req, account, &ts, &mut report) {
            tracing::warn!(user = %account.name, error = %e.error, "cannot carry this user's identity");
            report.failed.push(Problem {
                path: account.home.clone(),
                error: e.error,
            });
        }
    }
    report
}

/// Only a legacy dir `identity` has not migrated yet still holds one; without it a later `identity` carries no stale safe.
fn back_up_legacy_safe(legacy_home: &Path, from: &str) -> Result<Option<Move>, Failure> {
    let config_path = legacy_home.join(layout::LEGACY_CONFIG_DIR);
    let fail = |e: io::Error| Failure::new(Stage::Legacy, migrate::with_path(&config_path, &e));
    let home = match nofollow::open_home(legacy_home) {
        Ok(home) => home,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fail(e)),
    };
    let Some(config) = nofollow::find_dir(&home, layout::LEGACY_CONFIG_DIR.as_ref()).map_err(fail)? else {
        return Ok(None);
    };
    if !nofollow::exists(&config, layout::LEGACY_SAFE_FILE).map_err(fail)? {
        return Ok(None);
    }
    let aside = migrate::unique_name(&config, format!("{}.{from}.bak", layout::LEGACY_SAFE_FILE)).map_err(fail)?;
    nofollow::rename_noreplace(&config, layout::LEGACY_SAFE_FILE, &config, &aside).map_err(fail)?;
    nofollow::sync(&config).map_err(fail)?;
    let moved = Move {
        from: config_path.join(layout::LEGACY_SAFE_FILE),
        to: config_path.join(aside),
    };
    tracing::info!(from = %moved.from.display(), to = %moved.to.display(), "legacy safe of the retired network moved aside");
    Ok(Some(moved))
}

/// The legacy PIX db belongs to the retired network's pool like the safe; aside, a later `identity` cannot carry it on.
fn move_aside_legacy_pix(legacy_home: &Path, from: &str) -> Result<Option<Move>, Failure> {
    let db_path = legacy_home.join(layout::PIX_DB_FILE);
    let fail = |e: io::Error| Failure::new(Stage::Legacy, migrate::with_path(&db_path, &e));
    let home = match nofollow::open_home(legacy_home) {
        Ok(home) => home,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fail(e)),
    };
    if !nofollow::exists(&home, layout::PIX_DB_FILE).map_err(fail)? {
        return Ok(None);
    }
    let aside = migrate::unique_name(&home, format!("{}.{from}.bak", layout::PIX_DB_FILE)).map_err(fail)?;
    nofollow::rename_noreplace(&home, layout::PIX_DB_FILE, &home, &aside).map_err(fail)?;
    nofollow::sync(&home).map_err(fail)?;
    let moved = Move {
        from: db_path.clone(),
        to: legacy_home.join(aside),
    };
    tracing::info!(from = %moved.from.display(), to = %moved.to.display(), "legacy PIX db of the retired network moved aside");
    Ok(Some(moved))
}

fn switch_account(req: &SwitchRequest<'_>, account: &Account, ts: &str, report: &mut Report) -> Result<(), Failure> {
    let layout = layout::user_layout();
    let home = nofollow::open_home(&account.home).map_err(migrate::destination(&account.home))?;
    let from_rel = layout::network_dir(&layout.data, req.from);
    let from_path = account.home.join(&from_rel);
    let fail = migrate::destination(&from_path);
    let Some(from_data) = nofollow::find_dir(&home, &from_rel).map_err(&fail)? else {
        return Ok(());
    };
    if !nofollow::exists(&from_data, layout::IDENTITY_FILE).map_err(&fail)? {
        return Ok(());
    }
    if let Some(backup) = back_up_safe(&home, &layout, req.from, &from_data, &from_path, account)? {
        report.safe_backups.push(backup);
    }

    let networks = Networks::create(account, &layout)?;
    let to_path = networks.data_path.join(req.to);
    if has_identity(&networks.data, req.to).map_err(migrate::destination(&to_path))? {
        tracing::info!(user = %account.name, target = %to_path.display(), "user already has an identity on the new network, keeping it");
        report.kept.push(to_path);
        return Ok(());
    }
    let plan = plan(&from_data, &from_path)?;
    let staging = networks.free_staging(req.to, ts)?;
    // Whatever is there holds no identity, only state (e.g. a cached safe) that would bind the carried one to it.
    let replaced = networks.move_aside(req.to, "replaced", ts)?;
    let marker = migrate::marker_content(&from_path, req.toolkit_version);
    let mut created = Vec::new();
    let installed = migrate::stage(&networks, &staging, &plan, &marker, account, &mut created)
        .and_then(|()| migrate::publish(&networks, &staging, req.to));
    if let Err(e) = installed {
        networks.remove(&staging, &created);
        networks.restore(&replaced);
        return Err(e);
    }
    report.moved_aside.extend(replaced.into_iter().map(|(_, m)| m));
    tracing::info!(user = %account.name, from = %from_path.display(), to = %to_path.display(), "identity carried over");
    report.copied.push(Move {
        from: from_path.clone(),
        to: to_path,
    });
    Ok(())
}

/// Next to the old identity, outside the cache a cleaner may wipe: the old safe's address is what drains its funds.
fn back_up_safe(
    home: &OwnedFd,
    layout: &UserLayout,
    from: &str,
    from_data: &OwnedFd,
    from_path: &Path,
    account: &Account,
) -> Result<Option<Move>, Failure> {
    let cache_rel = layout::network_dir(&layout.cache, from);
    let safe_path = account.home.join(&cache_rel).join(layout::SAFE_FILE);
    let fail = migrate::destination(&safe_path);
    let Some(cache) = nofollow::find_dir(home, &cache_rel).map_err(&fail)? else {
        return Ok(None);
    };
    let mut safe = match nofollow::open_regular_file(&cache, layout::SAFE_FILE) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fail(e)),
    };
    let sha256 = migrate::sha256(&mut safe).map_err(&fail)?;
    let base = layout::safe_backup_file(from);
    if free_backup_name(from_data, &base, &sha256).map_err(&fail)?.is_none() {
        return Ok(None);
    }
    // Staged in a root-owned dir the account cannot enter, so the published bytes are the verified ones.
    let staging = migrate::unique_name(from_data, format!(".{base}.staging")).map_err(&fail)?;
    let staging_path = from_path.join(&staging);
    let staging_dir =
        nofollow::create_dir(from_data, &staging, layout::DIR_MODE).map_err(migrate::destination(&staging_path))?;
    let planned = Planned {
        from: safe_path.clone(),
        source: safe,
        place: Place::Data,
        rel: PathBuf::from(&base),
        sha256,
    };
    let published = migrate::copy_verified(&planned, &staging_dir, &staging_path, account).and_then(|()| {
        publish_backup(&staging_dir, &base, from_data, &sha256).map_err(migrate::destination(from_path))
    });
    remove_staging(from_data, &staging, &staging_dir, &base, &staging_path);
    let Some(name) = published? else {
        return Ok(None);
    };
    Ok(Some(Move {
        from: safe_path.clone(),
        to: from_path.join(name),
    }))
}

/// Never replaces a backup: when a concurrent run takes the name first, the series is searched again.
fn publish_backup(
    staging_dir: &OwnedFd,
    staged: &str,
    from_data: &OwnedFd,
    sha256: &[u8; 32],
) -> io::Result<Option<String>> {
    loop {
        let Some(name) = free_backup_name(from_data, staged, sha256)? else {
            return Ok(None);
        };
        match nofollow::rename_noreplace(staging_dir, staged, from_data, &name) {
            Ok(()) => {
                nofollow::sync(from_data)?;
                return Ok(Some(name));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Through the held handle and never recursive: the account may have swapped its own dir in at the staging name.
fn remove_staging(from_data: &OwnedFd, staging: &str, staging_dir: &OwnedFd, staged: &str, staging_path: &Path) {
    if let Err(e) = nofollow::remove(staging_dir, staged)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %staging_path.join(staged).display(), error = %e, "cannot remove the staged safe backup");
    }
    if !matches!(nofollow::remove_empty_dir(from_data, staging), Ok(true)) {
        tracing::warn!(path = %staging_path.display(), "safe backup staging dir left in place");
    }
}

/// The first free name in the `<base>`, `<base>.1`, … series, or `None` when a backup in it already holds these bytes.
fn free_backup_name(dir: &OwnedFd, base: &str, sha256: &[u8; 32]) -> io::Result<Option<String>> {
    let mut n = 0;
    loop {
        let name = if n == 0 {
            base.to_string()
        } else {
            format!("{base}.{n}")
        };
        if !nofollow::exists(dir, &name)? {
            return Ok(Some(name));
        }
        // Anything but a readable regular file is passed over, never followed.
        if let Ok(mut existing) = nofollow::open_regular_file(dir, &name)
            && migrate::sha256(&mut existing).is_ok_and(|backed_up| backed_up == *sha256)
        {
            return Ok(None);
        }
        n += 1;
    }
}

/// The id and its password only; a symlink or FIFO is refused, so root never reads a file the user points it at.
fn plan(from_data: &OwnedFd, from_path: &Path) -> Result<Vec<Planned>, Failure> {
    let mut plan = Vec::new();
    for name in [layout::IDENTITY_FILE, layout::PASSWORD_FILE] {
        let path = from_path.join(name);
        let fail = migrate::destination(&path);
        let mut source = match nofollow::open_regular_file(from_data, name) {
            Ok(file) => file,
            Err(e) if name == layout::PASSWORD_FILE && e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fail(e)),
        };
        let sha256 = migrate::sha256(&mut source).map_err(&fail)?;
        plan.push(Planned {
            from: path.clone(),
            source,
            place: Place::Data,
            rel: PathBuf::from(name),
            sha256,
        });
    }
    Ok(plan)
}

/// A symlink or file at `networks/<net>` holds no identity; it is moved aside like any other stray entry.
/// A `hopr.id` the client cannot use (a link, FIFO or dir) is an error, not an identity to keep or to replace silently.
fn has_identity(networks: &OwnedFd, network: &str) -> io::Result<bool> {
    let dir = match nofollow::find_dir(networks, network.as_ref()) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Ok(false),
        Err(e) if is_not_a_dir(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    match nofollow::open_regular_file(&dir, layout::IDENTITY_FILE) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io::Error::other(format!("{} is unusable: {e}", layout::IDENTITY_FILE))),
    }
}

fn is_not_a_dir(e: &io::Error) -> bool {
    let errno = rustix::io::Errno::from_io_error(e);
    matches!(errno, Some(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::{IdentityRequest, Outcome};
    use rustix::fs::Mode;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    const FROM: &str = "jura-staging";
    const TO: &str = "piz-palu-staging";

    struct Fixture {
        tmp: tempfile::TempDir,
        legacy: PathBuf,
        accounts: Vec<Account>,
    }

    fn fixture(users: &[&str]) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        fs::create_dir_all(&legacy).unwrap();
        let accounts = users
            .iter()
            .map(|name| {
                let home = tmp.path().join(name);
                fs::create_dir_all(&home).unwrap();
                Account {
                    name: name.to_string(),
                    uid: uzers::get_effective_uid(),
                    gid: uzers::get_effective_gid(),
                    home,
                }
            })
            .collect();
        Fixture { tmp, legacy, accounts }
    }

    fn data(account: &Account, network: &str) -> PathBuf {
        account
            .home
            .join(layout::network_dir(&layout::user_layout().data, network))
    }

    fn cache(account: &Account, network: &str) -> PathBuf {
        account
            .home
            .join(layout::network_dir(&layout::user_layout().cache, network))
    }

    /// The per-user layout an earlier `identity` run (or the client) left on the old network.
    fn on_old_network(account: &Account) {
        let data = data(account, FROM);
        fs::create_dir_all(data.join(layout::PIX_DIR)).unwrap();
        fs::write(data.join(layout::IDENTITY_FILE), format!("{} id", account.name)).unwrap();
        fs::write(data.join(layout::PASSWORD_FILE), format!("{} pass", account.name)).unwrap();
        fs::write(data.join(layout::PIX_DIR).join(layout::PIX_DB_FILE), "jura pix").unwrap();
        fs::create_dir_all(cache(account, FROM)).unwrap();
        fs::write(cache(account, FROM).join(layout::SAFE_FILE), "jura safe").unwrap();
    }

    impl Fixture {
        fn run(&self) -> Report {
            switch_network(&SwitchRequest {
                from: FROM,
                to: TO,
                legacy_home: &self.legacy,
                accounts: &self.accounts,
                toolkit_version: "0.6.0-test",
            })
        }
    }

    fn mode(path: &Path) -> Mode {
        let stat = rustix::fs::lstat(path).unwrap();
        Mode::from_raw_mode(stat.st_mode) & Mode::from_bits_truncate(0o777)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn carries_the_identity_and_leaves_the_safe_and_pix_behind() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        let report = fx.run();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.copied.len(), 1);

        let new = data(alice, TO);
        assert_eq!(fs::read_to_string(new.join(layout::IDENTITY_FILE)).unwrap(), "alice id");
        assert_eq!(
            fs::read_to_string(new.join(layout::PASSWORD_FILE)).unwrap(),
            "alice pass"
        );
        assert_eq!(
            names(&new),
            [layout::IDENTITY_FILE, layout::PASSWORD_FILE, layout::MIGRATED_FROM_FILE]
        );
        let marker = fs::read_to_string(new.join(layout::MIGRATED_FROM_FILE)).unwrap();
        assert!(marker.contains(&data(alice, FROM).display().to_string()), "{marker}");
        assert!(names(&cache(alice, TO)).is_empty(), "the client writes the new safe");
        assert_eq!(mode(&new), layout::DIR_MODE);
        assert_eq!(mode(&new.join(layout::IDENTITY_FILE)), layout::FILE_MODE);

        let old = data(alice, FROM);
        assert_eq!(
            names(&old),
            [
                layout::IDENTITY_FILE,
                layout::PASSWORD_FILE,
                "hopr.safe.jura-staging.bak",
                layout::PIX_DIR,
            ]
        );
        assert_eq!(
            fs::read_to_string(old.join(layout::safe_backup_file(FROM))).unwrap(),
            "jura safe"
        );
        assert_eq!(mode(&old.join(layout::safe_backup_file(FROM))), layout::FILE_MODE);
        assert_eq!(
            fs::read_to_string(cache(alice, FROM).join(layout::SAFE_FILE)).unwrap(),
            "jura safe"
        );
        assert_eq!(report.safe_backups.len(), 1);
    }

    #[test]
    fn a_rerun_changes_nothing_and_a_changed_safe_gets_its_own_backup() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        fx.run();
        let rerun = fx.run();
        assert!(rerun.copied.is_empty() && rerun.safe_backups.is_empty() && rerun.failed.is_empty());
        assert_eq!(rerun.kept, [data(alice, TO)]);

        fs::write(cache(alice, FROM).join(layout::SAFE_FILE), "redeployed jura safe").unwrap();
        assert_eq!(fx.run().safe_backups.len(), 1);
        let backup = data(alice, FROM).join(format!("{}.1", layout::safe_backup_file(FROM)));
        assert_eq!(fs::read_to_string(backup).unwrap(), "redeployed jura safe");
        assert!(fx.run().safe_backups.is_empty(), "a numbered backup counts as done too");
        assert!(
            !data(alice, FROM)
                .join(format!("{}.2", layout::safe_backup_file(FROM)))
                .exists()
        );
    }

    #[test]
    fn a_failed_safe_backup_leaves_no_partial_file() {
        // Root may chown to any uid, so only an unprivileged run fails the copy.
        if crate::user::is_root() {
            return;
        }
        let mut fx = fixture(&["alice"]);
        fx.accounts[0].uid += 1;
        let alice = &fx.accounts[0];
        on_old_network(alice);
        let report = fx.run();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            names(&data(alice, FROM)),
            [layout::IDENTITY_FILE, layout::PASSWORD_FILE, layout::PIX_DIR]
        );
    }

    #[test]
    fn an_unusable_identity_on_the_new_network_fails_that_user_instead_of_counting_as_kept() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        fs::create_dir_all(data(alice, TO)).unwrap();
        std::os::unix::fs::symlink(
            fx.tmp.path().join("nowhere"),
            data(alice, TO).join(layout::IDENTITY_FILE),
        )
        .unwrap();
        let report = fx.run();
        assert!(report.kept.is_empty() && report.copied.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(
            report.failed[0].error.contains("unusable"),
            "{}",
            report.failed[0].error
        );
    }

    #[test]
    fn a_user_with_an_identity_on_the_new_network_keeps_it() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        fs::create_dir_all(data(alice, TO)).unwrap();
        fs::write(data(alice, TO).join(layout::IDENTITY_FILE), "piz-palu id").unwrap();
        let report = fx.run();
        assert!(report.copied.is_empty());
        assert_eq!(report.kept, [data(alice, TO)]);
        assert_eq!(
            fs::read_to_string(data(alice, TO).join(layout::IDENTITY_FILE)).unwrap(),
            "piz-palu id"
        );
        assert_eq!(report.safe_backups.len(), 1, "the old safe is backed up regardless");
    }

    #[test]
    fn a_stray_safe_on_the_new_network_is_moved_aside() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        fs::create_dir_all(cache(alice, TO)).unwrap();
        fs::write(cache(alice, TO).join(layout::SAFE_FILE), "orphan safe").unwrap();
        let report = fx.run();
        assert_eq!(report.copied.len(), 1);
        assert_eq!(report.moved_aside.len(), 1);
        assert_eq!(report.moved_aside[0].from, cache(alice, TO));
        assert!(names(&cache(alice, TO)).is_empty());
        let aside = &report.moved_aside[0].to;
        assert_eq!(
            fs::read_to_string(aside.join(layout::SAFE_FILE)).unwrap(),
            "orphan safe"
        );
    }

    #[test]
    fn users_without_an_identity_on_the_old_network_are_untouched() {
        let fx = fixture(&["alice", "bob"]);
        fs::create_dir_all(data(&fx.accounts[1], FROM)).unwrap();
        let report = fx.run();
        assert!(report.is_empty(), "{report:?}");
        assert!(names(&fx.accounts[0].home).is_empty());
        assert!(!data(&fx.accounts[1], TO).exists());
    }

    #[test]
    fn the_legacy_safe_and_pix_db_are_moved_aside_only_while_the_legacy_dir_still_holds_them() {
        let fx = fixture(&[]);
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join(layout::LEGACY_IDENTITY_FILE), "legacy id").unwrap();
        fs::write(config.join(layout::LEGACY_SAFE_FILE), "legacy jura safe").unwrap();
        fs::write(fx.legacy.join(layout::PIX_DB_FILE), "legacy jura pix").unwrap();
        let report = fx.run();
        let backup = config.join(format!("{}.{FROM}.bak", layout::LEGACY_SAFE_FILE));
        assert_eq!(report.safe_backups[0].to, backup);
        assert_eq!(fs::read_to_string(&backup).unwrap(), "legacy jura safe");
        assert!(!config.join(layout::LEGACY_SAFE_FILE).exists());
        assert!(config.join(layout::LEGACY_IDENTITY_FILE).exists());
        let pix = fx.legacy.join(format!("{}.{FROM}.bak", layout::PIX_DB_FILE));
        assert_eq!(report.moved_aside[0].to, pix);
        assert_eq!(fs::read_to_string(&pix).unwrap(), "legacy jura pix");
        assert!(!fx.legacy.join(layout::PIX_DB_FILE).exists());
        assert!(fx.run().is_empty());
    }

    #[test]
    fn another_runs_staging_dir_survives_a_failed_switch() {
        // Root may chown to any uid, so only an unprivileged run fails mid-stage.
        if crate::user::is_root() {
            return;
        }
        let mut fx = fixture(&["alice"]);
        fx.accounts[0].uid += 1;
        let alice = &fx.accounts[0];
        on_old_network(alice);
        fs::remove_file(cache(alice, FROM).join(layout::SAFE_FILE)).unwrap();
        let networks = data(alice, TO).parent().unwrap().to_path_buf();
        let now = chrono::Utc::now();
        let theirs: Vec<PathBuf> = (0..5)
            .map(|offset| {
                let ts = (now + chrono::Duration::seconds(offset)).format("%Y%m%d%H%M%S");
                networks.join(format!("{TO}.migrating-{ts}"))
            })
            .collect();
        for dir in &theirs {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(layout::IDENTITY_FILE), "another run's id").unwrap();
        }
        assert_eq!(fx.run().failed.len(), 1);
        for dir in &theirs {
            assert_eq!(
                fs::read_to_string(dir.join(layout::IDENTITY_FILE)).unwrap(),
                "another run's id"
            );
        }
        assert!(!data(alice, TO).exists());
    }

    #[test]
    fn a_planted_symlink_at_the_new_network_is_moved_aside_not_written_through() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        on_old_network(alice);
        let outside = fx.tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, data(alice, TO)).unwrap();
        let report = fx.run();
        assert_eq!(report.copied.len(), 1, "{:?}", report.failed);
        assert!(names(&outside).is_empty());
        assert!(
            fs::symlink_metadata(&report.moved_aside[0].to)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(data(alice, TO).join(layout::IDENTITY_FILE).is_file());
    }

    #[test]
    fn a_symlinked_identity_fails_that_user_and_the_others_still_switch() {
        let fx = fixture(&["alice", "bob"]);
        let (alice, bob) = (&fx.accounts[0], &fx.accounts[1]);
        on_old_network(alice);
        on_old_network(bob);
        let secret = fx.tmp.path().join("root-only");
        fs::write(&secret, "not alice's").unwrap();
        fs::remove_file(data(alice, FROM).join(layout::IDENTITY_FILE)).unwrap();
        std::os::unix::fs::symlink(&secret, data(alice, FROM).join(layout::IDENTITY_FILE)).unwrap();
        let report = fx.run();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].path, alice.home);
        assert!(!data(alice, TO).exists());
        assert_eq!(report.copied.len(), 1);
        assert_eq!(
            fs::read_to_string(data(bob, TO).join(layout::IDENTITY_FILE)).unwrap(),
            "bob id"
        );
        assert_eq!(Outcome::Switched(report).exit_code(), exitcode::CANTCREAT);
    }

    #[test]
    fn a_legacy_identity_migrated_onto_the_old_network_reaches_the_new_one_without_its_safe() {
        let fx = fixture(&["alice"]);
        let alice = &fx.accounts[0];
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        fs::create_dir_all(&config).unwrap();
        for (name, bytes) in [
            (layout::LEGACY_IDENTITY_FILE, "legacy id"),
            (layout::LEGACY_PASSWORD_FILE, "legacy pass"),
            (layout::LEGACY_SAFE_FILE, "legacy jura safe"),
        ] {
            fs::write(config.join(name), bytes).unwrap();
        }
        let migrated = migrate::migrate_identity(&IdentityRequest {
            account: alice,
            network: FROM,
            legacy_home: &fx.legacy,
            dry_run: false,
            replace: false,
            toolkit_version: "0.6.0-test",
        })
        .unwrap();
        assert!(matches!(migrated, Outcome::Migrated { .. }));

        let report = fx.run();
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(
            fs::read_to_string(data(alice, TO).join(layout::IDENTITY_FILE)).unwrap(),
            "legacy id"
        );
        assert!(names(&cache(alice, TO)).is_empty());
        assert_eq!(
            fs::read_to_string(cache(alice, FROM).join(layout::SAFE_FILE)).unwrap(),
            "legacy jura safe"
        );
        assert_eq!(
            fs::read_to_string(data(alice, FROM).join(layout::safe_backup_file(FROM))).unwrap(),
            "legacy jura safe"
        );
    }

    /// Only `just test-root` reaches this: chown to a foreign uid needs root.
    #[test]
    fn as_root_everything_created_belongs_to_the_account() {
        if !crate::user::is_root() {
            return;
        }
        let nobody = uzers::get_user_by_name("nobody").expect("a nobody account");
        let mut fx = fixture(&["alice"]);
        fx.accounts[0].uid = nobody.uid();
        fx.accounts[0].gid = nobody.primary_group_id();
        let alice = &fx.accounts[0];
        on_old_network(alice);
        assert!(fx.run().failed.is_empty());
        let created = [
            data(alice, TO),
            data(alice, TO).join(layout::IDENTITY_FILE),
            data(alice, TO).join(layout::PASSWORD_FILE),
            data(alice, TO).join(layout::MIGRATED_FROM_FILE),
            cache(alice, TO),
            data(alice, FROM).join(layout::safe_backup_file(FROM)),
        ];
        for path in created {
            let meta = fs::symlink_metadata(&path).unwrap();
            assert_eq!((meta.uid(), meta.gid()), (alice.uid, alice.gid), "{}", path.display());
        }
    }

    fn open(path: &Path) -> OwnedFd {
        nofollow::open_home(path).unwrap()
    }

    #[test]
    fn a_backup_published_meanwhile_is_never_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let staging = data.join(".staging");
        fs::create_dir_all(&staging).unwrap();
        let base = layout::safe_backup_file(FROM);
        fs::write(staging.join(&base), "this run's safe").unwrap();
        // A concurrent run published its backup after this run picked the name.
        fs::write(data.join(&base), "the other run's safe").unwrap();
        let rename = nofollow::rename_noreplace(&open(&staging), &base, &open(&data), &base);
        assert_eq!(rename.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        let sha256 = migrate::sha256(fs::File::open(staging.join(&base)).unwrap()).unwrap();
        let name = publish_backup(&open(&staging), &base, &open(&data), &sha256).unwrap();
        assert_eq!(name, Some(format!("{base}.1")));
        assert_eq!(fs::read_to_string(data.join(&base)).unwrap(), "the other run's safe");
        assert_eq!(
            fs::read_to_string(data.join(format!("{base}.1"))).unwrap(),
            "this run's safe"
        );
    }

    #[test]
    fn a_dir_swapped_in_at_the_staging_name_survives_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir_all(data.join(".staging")).unwrap();
        fs::write(data.join(".staging").join("staged"), "safe").unwrap();
        let staging_dir = open(&data.join(".staging"));
        fs::rename(data.join(".staging"), tmp.path().join("ours")).unwrap();
        fs::create_dir(data.join(".staging")).unwrap();
        fs::write(data.join(".staging").join("theirs"), "the account's file").unwrap();

        remove_staging(&open(&data), ".staging", &staging_dir, "staged", &data.join(".staging"));
        assert_eq!(names(&data.join(".staging")), ["theirs"]);
        assert!(
            names(&tmp.path().join("ours")).is_empty(),
            "the staged file goes through the handle"
        );
    }

    #[test]
    fn json_and_exit_code_of_a_clean_run() {
        let report = Report {
            from: FROM.into(),
            to: TO.into(),
            ..Report::default()
        };
        let outcome = Outcome::Switched(report);
        assert_eq!(outcome.exit_code(), 0);
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["type"], "Switched");
        assert_eq!(json["to"], TO);
    }
}
