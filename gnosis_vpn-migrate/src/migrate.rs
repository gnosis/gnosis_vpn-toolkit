//! The migration: enumerate, compare, copy into a staging dir, verify, rename into place. Nothing is ever deleted.
//! Root writes into the user's home only through `nofollow`, so a symlink the user plants cannot redirect them.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use exitcode::ExitCode;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::layout::{self, UserLayout};
use crate::nofollow;
use crate::user::{self, Account};

/// Where a run failed; each stage has one exit code, which is what the installer scripts key on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum Stage {
    Privileges,
    User,
    Network,
    Legacy,
    Destination,
    Upload,
}

impl Stage {
    pub fn exit_code(self) -> ExitCode {
        match self {
            Stage::Privileges => exitcode::NOPERM,
            Stage::User => exitcode::USAGE,
            Stage::Network => exitcode::DATAERR,
            Stage::Legacy => exitcode::NOINPUT,
            Stage::Destination => exitcode::CANTCREAT,
            Stage::Upload => exitcode::SOFTWARE,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            Stage::Privileges => "privileges",
            Stage::User => "user",
            Stage::Network => "network",
            Stage::Legacy => "legacy files",
            Stage::Destination => "destination",
            Stage::Upload => "upload",
        };
        f.write_str(word)
    }
}

#[derive(Debug)]
pub struct Failure {
    pub stage: Stage,
    pub error: String,
}

impl Failure {
    pub fn new(stage: Stage, error: impl fmt::Display) -> Self {
        Self {
            stage,
            error: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum Difference {
    Missing,
    Differs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub legacy: PathBuf,
    pub target: PathBuf,
    pub difference: Difference,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum Outcome {
    Migrated {
        network: String,
        user: String,
        files: Vec<Move>,
        legacy: Vec<Move>,
    },
    WouldMigrate {
        network: String,
        user: String,
        files: Vec<Move>,
    },
    AlreadyMigrated {
        network: String,
        user: String,
        legacy: Vec<Move>,
    },
    NothingToMigrate {
        legacy_home: PathBuf,
    },
    Conflict {
        network: String,
        user: String,
        mismatches: Vec<Mismatch>,
    },
    StartedClean {
        moved: Vec<Move>,
    },
    Relocated(crate::relocate::Report),
    NothingToDo,
    Uploaded {
        uuid: String,
    },
    Failed {
        stage: Stage,
        error: String,
    },
}

/// The one stdout line of a run (`--output json`).
#[derive(Debug, Serialize)]
pub struct MigrateResult<'a> {
    pub outcome: &'a Outcome,
}

impl Outcome {
    pub fn from_failure(failure: Failure) -> Self {
        Outcome::Failed {
            stage: failure.stage,
            error: failure.error,
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Outcome::Conflict { .. } => exitcode::TEMPFAIL,
            Outcome::Failed { stage, .. } => stage.exit_code(),
            _ => exitcode::OK,
        }
    }

    /// The one-line reason for a non-OK exit.
    pub fn reason(&self) -> String {
        match self {
            Outcome::Conflict { network, user, .. } => {
                format!("user {user} already has a different identity on network {network}")
            }
            Outcome::Failed { stage, error } => format!("{stage}: {error}"),
            _ => "ok".to_string(),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Migrated {
                network,
                user,
                files,
                legacy,
            } => {
                writeln!(f, "Migrated the legacy identity for user {user} on network {network}:")?;
                write_moves(f, files)?;
                writeln!(f, "Legacy files moved aside:")?;
                write_moves(f, legacy)
            }
            Outcome::WouldMigrate { network, user, files } => {
                writeln!(
                    f,
                    "Dry run: would migrate the legacy identity for user {user} on network {network}:"
                )?;
                write_moves(f, files)
            }
            Outcome::AlreadyMigrated { network, user, legacy } if legacy.is_empty() => {
                write!(
                    f,
                    "Identity already migrated for user {user} on network {network}; a run moves the legacy files aside"
                )
            }
            Outcome::AlreadyMigrated { network, user, legacy } => {
                writeln!(f, "Identity already migrated for user {user} on network {network}")?;
                writeln!(f, "Legacy files moved aside:")?;
                write_moves(f, legacy)
            }
            Outcome::NothingToMigrate { legacy_home } => {
                write!(
                    f,
                    "No legacy identity under {}; nothing to migrate",
                    legacy_home.display()
                )
            }
            Outcome::Conflict {
                network,
                user,
                mismatches,
            } => {
                writeln!(
                    f,
                    "User {user} already has an identity on network {network} that differs from the legacy one:"
                )?;
                for m in mismatches {
                    let what = match m.difference {
                        Difference::Missing => "missing",
                        Difference::Differs => "differs",
                    };
                    writeln!(f, "  {} -> {}: {what}", m.legacy.display(), m.target.display())?;
                }
                write!(
                    f,
                    "Re-run with --replace to keep the legacy identity (the current one moves to networks/{network}.replaced-<timestamp>/), or run start-clean to keep the current one."
                )
            }
            Outcome::StartedClean { moved } => {
                writeln!(f, "Moved aside for a fresh start:")?;
                write_moves(f, moved)
            }
            Outcome::NothingToDo => write!(f, "Nothing to move aside"),
            Outcome::Relocated(report) if report.is_empty() => write!(f, "Layout already current; nothing to move"),
            Outcome::Relocated(report) => {
                writeln!(f, "Moved into the current layout:")?;
                write_moves(f, &report.moved)?;
                for path in &report.removed {
                    writeln!(f, "  removed {}", path.display())?;
                }
                for problem in &report.failed {
                    writeln!(f, "  FAILED {}: {}", problem.path.display(), problem.error)?;
                }
                Ok(())
            }
            Outcome::Uploaded { uuid } => {
                write!(
                    f,
                    "Uploaded the migration log bundle; reference: {uuid}\nQuote this reference when reporting the problem."
                )
            }
            Outcome::Failed { stage, error } => write!(f, "Failed ({stage}): {error}"),
        }
    }
}

fn write_moves(f: &mut fmt::Formatter<'_>, moves: &[Move]) -> fmt::Result {
    for m in moves {
        writeln!(f, "  {} -> {}", m.from.display(), m.to.display())?;
    }
    Ok(())
}

pub struct IdentityRequest<'a> {
    pub account: &'a Account,
    pub network: &'a str,
    pub legacy_home: &'a Path,
    pub dry_run: bool,
    pub replace: bool,
    pub toolkit_version: &'a str,
}

pub fn legacy_identity_exists(legacy_home: &Path) -> Result<bool, Failure> {
    Ok(Legacy::find(legacy_home)?.is_some())
}

pub fn migrate_identity(req: &IdentityRequest<'_>) -> Result<Outcome, Failure> {
    let Some(legacy) = Legacy::find(req.legacy_home)? else {
        tracing::info!(legacy_home = %req.legacy_home.display(), "no legacy identity found");
        return Ok(Outcome::NothingToMigrate {
            legacy_home: req.legacy_home.to_path_buf(),
        });
    };
    let plan = legacy.plan()?;
    let layout = layout::user_layout();
    let target = Target::new(&req.account.home, &layout, req.network);
    let files: Vec<Move> = plan
        .iter()
        .map(|p| Move {
            from: p.from.clone(),
            to: target.path(p),
        })
        .collect();
    let network = req.network.to_string();
    let user = req.account.name.clone();
    tracing::info!(%network, %user, files = plan.len(), target = %target.data.display(), "legacy identity found");

    if target.exists()? {
        let mismatches = target.compare(&plan)?;
        if mismatches.is_empty() {
            tracing::info!(target = %target.data.display(), "destination already holds the legacy identity");
            // An earlier run stopped after publishing, so the old key is still readable by the service user.
            let legacy = if req.dry_run {
                Vec::new()
            } else {
                legacy.move_aside(&timestamp())?
            };
            return Ok(Outcome::AlreadyMigrated { network, user, legacy });
        }
        for m in &mismatches {
            tracing::warn!(legacy = %m.legacy.display(), target = %m.target.display(), difference = ?m.difference, "destination differs");
        }
        if !req.replace {
            return Ok(Outcome::Conflict {
                network,
                user,
                mismatches,
            });
        }
    }
    if req.dry_run {
        return Ok(Outcome::WouldMigrate { network, user, files });
    }

    let ts = timestamp();
    let networks = Networks::create(req.account, &layout)?;
    let replaced = networks.move_aside(req.network, "replaced", &ts)?;
    let staging = format!("{}.migrating-{ts}", req.network);
    let marker = marker_content(&legacy, req.toolkit_version);
    let installed = stage(&networks, &staging, &plan, &marker, req.account)
        .and_then(|()| publish(&networks, &staging, req.network));
    if let Err(e) = installed {
        tracing::warn!(error = %e.error, "migration failed, removing the staging dirs and restoring replaced ones");
        networks.remove(&staging);
        networks.restore(&replaced);
        return Err(e);
    }
    // Only once the new identity is durable may the legacy one disappear.
    let legacy_moves = legacy.move_aside(&ts)?;
    tracing::info!(%network, %user, "migration complete");
    Ok(Outcome::Migrated {
        network,
        user,
        files,
        legacy: legacy_moves,
    })
}

/// Moves the legacy identity aside and, with an account, that user's per-network dirs, so the next start generates a fresh identity.
pub fn start_clean(legacy_home: &Path, user_dirs: Option<(&Account, &str)>) -> Result<Outcome, Failure> {
    let ts = timestamp();
    let mut moved = Vec::new();
    if let Some(config_move) = move_legacy_config_aside(legacy_home, &ts)? {
        moved.push(config_move);
    }
    if let Some((account, network)) = user_dirs {
        moved.extend(move_user_dirs_aside(account, network, &ts)?);
    }
    if moved.is_empty() {
        return Ok(Outcome::NothingToDo);
    }
    Ok(Outcome::StartedClean { moved })
}

/// Same lock-down as a migration (GNO-1006): the `.bak` keeps the old key and password.
fn move_legacy_config_aside(legacy_home: &Path, ts: &str) -> Result<Option<Move>, Failure> {
    let home = match nofollow::open_home(legacy_home) {
        Ok(home) => home,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(legacy_failure(legacy_home, &e)),
    };
    let config_dir = legacy_home.join(layout::LEGACY_CONFIG_DIR);
    let Some(config) =
        nofollow::find_dir(&home, layout::LEGACY_CONFIG_DIR.as_ref()).map_err(|e| legacy_failure(&config_dir, &e))?
    else {
        return Ok(None);
    };
    let aside = format!("{}.{ts}.bak", layout::LEGACY_CONFIG_DIR);
    let config_move = rename_aside(&home, legacy_home, layout::LEGACY_CONFIG_DIR, aside)?;
    lock_down(&config).map_err(|e| legacy_failure(&config_move.to, &e))?;
    nofollow::sync(&home).map_err(|e| legacy_failure(legacy_home, &e))?;
    Ok(Some(config_move))
}

/// Only opens existing dirs: a reset must not create the per-user tree.
fn move_user_dirs_aside(account: &Account, network: &str, ts: &str) -> Result<Vec<Move>, Failure> {
    let layout = layout::user_layout();
    let home = nofollow::open_home(&account.home).map_err(destination(&account.home))?;
    let mut moved = Vec::new();
    for base in [&layout.data, &layout.cache] {
        let rel = base.join(layout::NETWORKS_DIR);
        let path = account.home.join(&rel);
        let Some(networks) = nofollow::find_dir(&home, &rel).map_err(destination(&path))? else {
            continue;
        };
        moved.extend(move_aside_in(&networks, &path, network, "reset", ts)?);
    }
    Ok(moved)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Data,
    Cache,
}

struct Planned {
    from: PathBuf,
    source: File,
    place: Place,
    rel: PathBuf,
    sha256: [u8; 32],
}

struct Source {
    from: PathBuf,
    in_config: bool,
    name: &'static str,
    place: Place,
    rel: PathBuf,
}

/// `.config/gnosisvpn-hopr.{id,pass,safe}` and the PIX db; the home is service-owned, so root reads only through no-follow handles.
struct Legacy {
    home: OwnedFd,
    home_path: PathBuf,
    config: OwnedFd,
    config_dir: PathBuf,
    owner_uid: u32,
    has_redb: bool,
    sources: Vec<Source>,
}

impl Legacy {
    fn find(home_path: &Path) -> Result<Option<Self>, Failure> {
        let config_dir = home_path.join(layout::LEGACY_CONFIG_DIR);
        let home = match nofollow::open_home(home_path) {
            Ok(home) => home,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(legacy_failure(home_path, &e)),
        };
        let Some(config) = nofollow::find_dir(&home, layout::LEGACY_CONFIG_DIR.as_ref())
            .map_err(|e| legacy_failure(&config_dir, &e))?
        else {
            return Ok(None);
        };
        // Read before anything moves: the sweep after a migration takes this account's files away.
        let owner_uid = rustix::fs::fstat(&config)
            .map_err(|e| legacy_failure(&config_dir, &e.into()))?
            .st_uid;
        let config_has =
            |name: &str| nofollow::exists(&config, name).map_err(|e| legacy_failure(&config_dir.join(name), &e));
        if !config_has(layout::LEGACY_IDENTITY_FILE)? {
            return Ok(None);
        }
        let source = |in_config: bool, name: &'static str, place: Place, rel: PathBuf| Source {
            from: if in_config {
                config_dir.join(name)
            } else {
                home_path.join(name)
            },
            in_config,
            name,
            place,
            rel,
        };
        let mut sources = vec![source(
            true,
            layout::LEGACY_IDENTITY_FILE,
            Place::Data,
            PathBuf::from(layout::IDENTITY_FILE),
        )];
        if config_has(layout::LEGACY_PASSWORD_FILE)? {
            sources.push(source(
                true,
                layout::LEGACY_PASSWORD_FILE,
                Place::Data,
                PathBuf::from(layout::PASSWORD_FILE),
            ));
        }
        if config_has(layout::LEGACY_SAFE_FILE)? {
            sources.push(source(
                true,
                layout::LEGACY_SAFE_FILE,
                Place::Cache,
                PathBuf::from(layout::SAFE_FILE),
            ));
        }
        let has_redb = nofollow::exists(&home, layout::PIX_DB_FILE)
            .map_err(|e| legacy_failure(&home_path.join(layout::PIX_DB_FILE), &e))?;
        if has_redb {
            let rel = Path::new(layout::PIX_DIR).join(layout::PIX_DB_FILE);
            sources.push(source(false, layout::PIX_DB_FILE, Place::Data, rel));
        }
        Ok(Some(Self {
            home,
            home_path: home_path.to_path_buf(),
            config,
            config_dir,
            owner_uid,
            has_redb,
            sources,
        }))
    }

    /// Hashing doubles as the readability check, so an unreadable legacy file fails before anything is created.
    fn plan(&self) -> Result<Vec<Planned>, Failure> {
        self.sources
            .iter()
            .map(|s| {
                let dir = if s.in_config { &self.config } else { &self.home };
                let mut source = nofollow::open_regular_file(dir, s.name).map_err(|e| legacy_failure(&s.from, &e))?;
                let sha256 = sha256(&mut source).map_err(|e| legacy_failure(&s.from, &e))?;
                Ok(Planned {
                    from: s.from.clone(),
                    source,
                    place: s.place,
                    rel: s.rel.clone(),
                    sha256,
                })
            })
            .collect()
    }

    fn move_aside(&self, ts: &str) -> Result<Vec<Move>, Failure> {
        let mut moved = Vec::new();
        let config_aside = format!("{}.migrated-{ts}", layout::LEGACY_CONFIG_DIR);
        let config_move = rename_aside(&self.home, &self.home_path, layout::LEGACY_CONFIG_DIR, config_aside)?;
        lock_down(&self.config).map_err(|e| legacy_failure(&config_move.to, &e))?;
        moved.push(config_move);
        if self.has_redb {
            let aside = format!("{}.migrated-{ts}", layout::PIX_DB_FILE);
            moved.push(rename_aside(&self.home, &self.home_path, layout::PIX_DB_FILE, aside)?);
        }
        secure_leftovers(&self.home, &self.home_path, self.owner_uid);
        nofollow::sync(&self.home).map_err(|e| legacy_failure(&self.home_path, &e))?;
        Ok(moved)
    }
}

/// Keeps the old key and password from the service user and everyone else (GNO-1006); euid is root outside tests.
fn lock_down(dir: &OwnedFd) -> io::Result<()> {
    rustix::fs::fchown(dir, Some(rustix::process::geteuid()), Some(rustix::process::getegid()))?;
    rustix::fs::fchmod(dir, layout::DIR_MODE)?;
    Ok(())
}

/// The installer deletes the service account and a later one may reuse its uid (GNO-960), so root takes what it owned.
/// Best effort: a rerun finds nothing to migrate and never sweeps again, so one bad entry must not stop the rest.
fn secure_leftovers(home: &OwnedFd, home_path: &Path, legacy_uid: u32) -> Vec<PathBuf> {
    let names = match nofollow::entries(home) {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(path = %home_path.display(), error = %e, "cannot list the legacy home to secure it");
            return Vec::new();
        }
    };
    let mut secured = Vec::new();
    for name in names {
        let path = home_path.join(&name);
        match secure_entry(home, &name, legacy_uid) {
            Ok((false, 0)) => {}
            Ok((locked, reowned)) => {
                tracing::info!(path = %path.display(), locked, reowned, "legacy entry secured");
                secured.push(path);
            }
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "cannot secure legacy entry"),
        }
    }
    secured
}

fn secure_entry(home: &OwnedFd, name: &OsStr, legacy_uid: u32) -> io::Result<(bool, usize)> {
    let locked = lock_config_dir(home, name)?;
    // A root-owned `.config` names no service account, and re-owning uid 0 would strip the updater's root:gnosisvpn groups.
    if legacy_uid == 0 {
        return Ok((locked, 0));
    }
    let reowned = nofollow::reown_tree(home, name, legacy_uid)?;
    Ok((locked, reowned))
}

/// Every `.config*` dir may hold an identity: a leftover one, or a `start-clean` backup.
fn lock_config_dir(home: &OwnedFd, name: &OsStr) -> io::Result<bool> {
    if !name.as_bytes().starts_with(layout::LEGACY_CONFIG_DIR.as_bytes()) {
        return Ok(false);
    }
    let stat = rustix::fs::statat(home, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    let is_dir = rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory;
    if !is_dir {
        return Ok(false);
    }
    lock_down(&nofollow::open_dir(home, name)?)?;
    Ok(true)
}

fn rename_aside(home: &OwnedFd, home_path: &Path, name: &str, aside: String) -> Result<Move, Failure> {
    let from = home_path.join(name);
    let fail = |e: io::Error| legacy_failure(&from, &e);
    let aside = unique_name(home, aside).map_err(fail)?;
    nofollow::rename(home, name, &aside).map_err(fail)?;
    let to = home_path.join(aside);
    tracing::info!(from = %from.display(), to = %to.display(), "legacy files moved aside");
    Ok(Move { from, to })
}

fn legacy_failure(path: &Path, e: &io::Error) -> Failure {
    Failure::new(Stage::Legacy, with_path(path, e))
}

/// The per-network dir pair, absolute for reporting and home-relative for the read-only comparison.
struct Target {
    home: PathBuf,
    data_rel: PathBuf,
    cache_rel: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl Target {
    fn new(home: &Path, layout: &UserLayout, network: &str) -> Self {
        let data_rel = layout::network_dir(&layout.data, network);
        let cache_rel = layout::network_dir(&layout.cache, network);
        Self {
            home: home.to_path_buf(),
            data: home.join(&data_rel),
            cache: home.join(&cache_rel),
            data_rel,
            cache_rel,
        }
    }

    /// Through the home handle, so a symlink at `networks/<net>` counts as present whether or not it resolves.
    fn exists(&self) -> Result<bool, Failure> {
        let home = nofollow::open_home(&self.home).map_err(destination(&self.home))?;
        for (rel, path) in [(&self.data_rel, &self.data), (&self.cache_rel, &self.cache)] {
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
                continue;
            };
            let Some(dir) = nofollow::find_dir(&home, parent).map_err(destination(path))? else {
                continue;
            };
            if nofollow::exists(&dir, name).map_err(destination(path))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn path(&self, planned: &Planned) -> PathBuf {
        match planned.place {
            Place::Data => self.data.join(&planned.rel),
            Place::Cache => self.cache.join(&planned.rel),
        }
    }

    fn compare(&self, plan: &[Planned]) -> Result<Vec<Mismatch>, Failure> {
        let home = nofollow::open_home(&self.home).map_err(destination(&self.home))?;
        let mut mismatches = Vec::new();
        for planned in plan {
            let target = self.path(planned);
            let Some(file) = self.open(&home, planned, &target) else {
                mismatches.push(Mismatch {
                    legacy: planned.from.clone(),
                    target,
                    difference: Difference::Missing,
                });
                continue;
            };
            let hash = sha256(file).map_err(|e| Failure::new(Stage::Destination, with_path(&target, &e)))?;
            if hash != planned.sha256 {
                mismatches.push(Mismatch {
                    legacy: planned.from.clone(),
                    target,
                    difference: Difference::Differs,
                });
            }
        }
        Ok(mismatches)
    }

    /// A symlink or FIFO anywhere below the home reads as missing, so root never hashes a file the user points at.
    fn open(&self, home: &OwnedFd, planned: &Planned, target: &Path) -> Option<File> {
        let base = match planned.place {
            Place::Data => &self.data_rel,
            Place::Cache => &self.cache_rel,
        };
        let rel = base.join(&planned.rel);
        let (parent, name) = (rel.parent()?, rel.file_name()?);
        let unusable = |e: io::Error| {
            tracing::warn!(target = %target.display(), error = %e, "destination file unusable, treated as missing");
        };
        let dir = match nofollow::find_dir(home, parent) {
            Ok(Some(dir)) => dir,
            Ok(None) => return None,
            Err(e) => {
                unusable(e);
                return None;
            }
        };
        match nofollow::open_regular_file(&dir, name) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                unusable(e);
                None
            }
        }
    }
}

/// The data and cache `networks/` dirs, held open so no later step resolves the user-controlled path again.
struct Networks {
    data: OwnedFd,
    cache: OwnedFd,
    data_path: PathBuf,
    cache_path: PathBuf,
}

impl Networks {
    fn create(account: &Account, layout: &UserLayout) -> Result<Self, Failure> {
        let home = nofollow::open_home(&account.home).map_err(destination(&account.home))?;
        for (parent, mode) in &layout.parents {
            nofollow::ensure_dir(&home, parent, *mode, account).map_err(destination(&account.home.join(parent)))?;
        }
        let data_rel = layout.data.join(layout::NETWORKS_DIR);
        let cache_rel = layout.cache.join(layout::NETWORKS_DIR);
        let data_path = account.home.join(&data_rel);
        let cache_path = account.home.join(&cache_rel);
        let data =
            nofollow::ensure_dir(&home, &data_rel, layout::DIR_MODE, account).map_err(destination(&data_path))?;
        let cache =
            nofollow::ensure_dir(&home, &cache_rel, layout::DIR_MODE, account).map_err(destination(&cache_path))?;
        Ok(Self {
            data,
            cache,
            data_path,
            cache_path,
        })
    }

    fn side(&self, place: Place) -> (&OwnedFd, &Path) {
        match place {
            Place::Data => (&self.data, &self.data_path),
            Place::Cache => (&self.cache, &self.cache_path),
        }
    }

    /// All or nothing: a failed cache move puts the data dir back, so the identity is never left half aside.
    fn move_aside(&self, network: &str, reason: &str, ts: &str) -> Result<Vec<(Place, Move)>, Failure> {
        let mut moved = Vec::new();
        for place in [Place::Data, Place::Cache] {
            let (dir, path) = self.side(place);
            match move_aside_in(dir, path, network, reason, ts) {
                Ok(Some(m)) => moved.push((place, m)),
                Ok(None) => {}
                Err(e) => {
                    self.restore(&moved);
                    return Err(e);
                }
            }
        }
        Ok(moved)
    }

    /// Best effort: the run is already failing, so a failed undo is only logged.
    fn restore(&self, moved: &[(Place, Move)]) {
        for (place, m) in moved {
            let (dir, _) = self.side(*place);
            let (Some(original), Some(aside)) = (m.from.file_name(), m.to.file_name()) else {
                continue;
            };
            match nofollow::rename(dir, aside, original) {
                Ok(()) => tracing::info!(from = %m.to.display(), to = %m.from.display(), "replaced dir restored"),
                Err(e) => tracing::error!(path = %m.to.display(), error = %e, "cannot restore the replaced dir"),
            }
        }
    }

    fn remove(&self, staging: &str) {
        for place in [Place::Data, Place::Cache] {
            let (dir, path) = self.side(place);
            match nofollow::remove_tree(dir, staging) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %path.join(staging).display(), error = %e, "cannot remove staging dir"),
            }
        }
    }
}

fn move_aside_in(
    networks: &OwnedFd,
    networks_path: &Path,
    network: &str,
    reason: &str,
    ts: &str,
) -> Result<Option<Move>, Failure> {
    let from = networks_path.join(network);
    let dest = destination(&from);
    if !nofollow::exists(networks, network).map_err(&dest)? {
        return Ok(None);
    }
    let aside = unique_name(networks, format!("{network}.{reason}-{ts}")).map_err(&dest)?;
    nofollow::rename(networks, network, &aside).map_err(&dest)?;
    let to = networks_path.join(&aside);
    tracing::info!(from = %from.display(), to = %to.display(), "per-user network dir moved aside");
    Ok(Some(Move { from: from.clone(), to }))
}

/// Both staging dirs stay root-owned until filled, so the user cannot plant entries in them; data first, then cache.
fn stage(networks: &Networks, name: &str, plan: &[Planned], marker: &str, account: &Account) -> Result<(), Failure> {
    for place in [Place::Data, Place::Cache] {
        let (parent, parent_path) = networks.side(place);
        let path = parent_path.join(name);
        let dest = destination(&path);
        let dir = nofollow::create_dir(parent, name, layout::DIR_MODE).map_err(&dest)?;
        for planned in plan.iter().filter(|p| p.place == place) {
            copy_verified(planned, &dir, &path, account)?;
        }
        if place == Place::Data {
            write_marker(&dir, &path, marker, account)?;
        }
        user::chown(&dir, account).map_err(&dest)?;
        nofollow::sync(&dir).map_err(&dest)?;
    }
    Ok(())
}

/// Plain copy (rename cannot cross from /var/lib to /home), own, fsync, then hash the written bytes against the source.
fn copy_verified(planned: &Planned, staging: &OwnedFd, staging_path: &Path, account: &Account) -> Result<(), Failure> {
    let to = staging_path.join(&planned.rel);
    let dest = destination(&to);
    let subdir = planned.rel.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = match subdir {
        Some(subdir) => nofollow::ensure_dir(staging, subdir, layout::DIR_MODE, account).map_err(&dest)?,
        None => staging.try_clone().map_err(&dest)?,
    };
    let file_name = planned.rel.file_name().unwrap_or_default();
    let mut source = &planned.source;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|e| legacy_failure(&planned.from, &e))?;
    let mut copy = nofollow::create_file(&dir, file_name, layout::FILE_MODE).map_err(&dest)?;
    io::copy(&mut source, &mut copy).map_err(&dest)?;
    user::chown(&copy, account).map_err(&dest)?;
    copy.sync_all().map_err(&dest)?;
    nofollow::sync(&dir).map_err(&dest)?;
    copy.seek(SeekFrom::Start(0)).map_err(&dest)?;
    let copied = sha256(&mut copy).map_err(&dest)?;
    if copied != planned.sha256 {
        return Err(Failure::new(
            Stage::Destination,
            format!("{} differs from {} after copy", to.display(), planned.from.display()),
        ));
    }
    tracing::info!(from = %planned.from.display(), to = %to.display(), "copied and verified");
    Ok(())
}

fn marker_content(legacy: &Legacy, toolkit_version: &str) -> String {
    format!(
        "source = {}\nmigrated_at = {}\ntoolkit_version = {toolkit_version}\n",
        legacy.config_dir.display(),
        chrono::Utc::now().to_rfc3339(),
    )
}

fn write_marker(staging: &OwnedFd, staging_path: &Path, content: &str, account: &Account) -> Result<(), Failure> {
    let path = staging_path.join(layout::MIGRATED_FROM_FILE);
    let dest = destination(&path);
    let mut marker = nofollow::create_file(staging, layout::MIGRATED_FROM_FILE, layout::FILE_MODE).map_err(&dest)?;
    marker.write_all(content.as_bytes()).map_err(&dest)?;
    user::chown(&marker, account).map_err(&dest)?;
    marker.sync_all().map_err(&dest)
}

/// Data first; a failed cache rename takes the data back, so a retry starts from the same state.
fn publish(networks: &Networks, staging: &str, network: &str) -> Result<(), Failure> {
    let rename_failed = |path: &Path, e: io::Error| {
        Failure::new(
            Stage::Destination,
            format!("{} -> {network}: {e}", path.join(staging).display()),
        )
    };
    nofollow::rename(&networks.data, staging, network).map_err(|e| rename_failed(&networks.data_path, e))?;
    if let Err(e) = nofollow::rename(&networks.cache, staging, network) {
        if let Err(undo) = nofollow::rename(&networks.data, network, staging) {
            tracing::error!(error = %undo, "cannot take the published data dir back");
        }
        return Err(rename_failed(&networks.cache_path, e));
    }
    for place in [Place::Data, Place::Cache] {
        let (dir, path) = networks.side(place);
        nofollow::sync(dir).map_err(destination(path))?;
    }
    tracing::info!(network, "renamed into place");
    Ok(())
}

fn destination(path: &Path) -> impl Fn(io::Error) -> Failure + '_ {
    move |e| Failure::new(Stage::Destination, with_path(path, &e))
}

fn sha256(mut reader: impl io::Read) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.finalize().into())
}

fn with_path(path: &Path, e: &io::Error) -> String {
    format!("{}: {e}", path.display())
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d%H%M%S").to_string()
}

/// Two runs within one second must not rename onto each other.
fn unique_name(dir: &OwnedFd, candidate: String) -> io::Result<String> {
    if !nofollow::exists(dir, &candidate)? {
        return Ok(candidate);
    }
    let mut n = 1;
    loop {
        let name = format!("{candidate}.{n}");
        if !nofollow::exists(dir, &name)? {
            return Ok(name);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::Mode;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const NETWORK: &str = "jura";

    struct Fixture {
        _tmp: tempfile::TempDir,
        legacy: PathBuf,
        account: Account,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        fixture_in(tmp.path().join("legacy"), tmp.path().join("home"), tmp)
    }

    fn fixture_in(legacy: PathBuf, home: PathBuf, tmp: tempfile::TempDir) -> Fixture {
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&home).unwrap();
        let account = Account {
            name: "tester".to_string(),
            uid: uzers::get_effective_uid(),
            gid: uzers::get_effective_gid(),
            home,
        };
        Fixture {
            _tmp: tmp,
            legacy,
            account,
        }
    }

    impl Fixture {
        fn with_legacy(&self, names: &[&str]) -> &Self {
            let config = self.legacy.join(layout::LEGACY_CONFIG_DIR);
            fs::create_dir_all(&config).unwrap();
            for name in names {
                let path = match *name {
                    "redb" => self.legacy.join(layout::PIX_DB_FILE),
                    other => config.join(format!("gnosisvpn-hopr.{other}")),
                };
                fs::write(&path, format!("legacy {name} bytes")).unwrap();
            }
            self
        }

        /// The layout with absolute paths, which is what the assertions look at.
        fn layout(&self) -> UserLayout {
            let rel = layout::user_layout();
            let home = &self.account.home;
            UserLayout {
                data: home.join(rel.data),
                cache: home.join(rel.cache),
                parents: rel.parents.into_iter().map(|(p, m)| (home.join(p), m)).collect(),
            }
        }

        fn target(&self) -> Target {
            Target::new(&self.account.home, &layout::user_layout(), NETWORK)
        }

        fn run(&self, dry_run: bool, replace: bool) -> Result<Outcome, Failure> {
            migrate_identity(&IdentityRequest {
                account: &self.account,
                network: NETWORK,
                legacy_home: &self.legacy,
                dry_run,
                replace,
                toolkit_version: "0.5.0-test",
            })
        }
    }

    fn mode(path: &Path) -> Mode {
        let stat = rustix::fs::stat(path).unwrap();
        Mode::from_raw_mode(stat.st_mode) & Mode::from_bits_truncate(0o777)
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn tree(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(path.strip_prefix(root).unwrap().display().to_string());
                if path.is_dir() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn nothing_to_migrate_without_a_legacy_id() {
        let fx = fixture();
        assert!(matches!(
            fx.run(false, false).unwrap(),
            Outcome::NothingToMigrate { .. }
        ));
        fx.with_legacy(&["pass"]);
        assert!(matches!(
            fx.run(false, false).unwrap(),
            Outcome::NothingToMigrate { .. }
        ));
        assert!(entries(&fx.account.home).is_empty());
    }

    #[test]
    fn full_migration_copies_verifies_and_moves_the_legacy_aside() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe", "redb"]);
        let outcome = fx.run(false, false).unwrap();
        let Outcome::Migrated {
            network,
            user,
            files,
            legacy,
        } = outcome
        else {
            panic!("expected Migrated");
        };
        assert_eq!((network.as_str(), user.as_str()), (NETWORK, "tester"));
        assert_eq!(files.len(), 4);

        let target = fx.target();
        let expect = |path: PathBuf, content: &str| {
            assert_eq!(fs::read_to_string(&path).unwrap(), content, "{}", path.display());
            assert_eq!(mode(&path), layout::FILE_MODE, "{}", path.display());
            assert_eq!(fs::metadata(&path).unwrap().uid(), fx.account.uid);
        };
        let expect_dir = |dir: PathBuf| {
            assert_eq!(mode(&dir), layout::DIR_MODE, "{}", dir.display());
            assert_eq!(fs::metadata(&dir).unwrap().uid(), fx.account.uid);
        };
        expect(target.data.join(layout::IDENTITY_FILE), "legacy id bytes");
        expect(target.data.join(layout::PASSWORD_FILE), "legacy pass bytes");
        expect(target.cache.join(layout::SAFE_FILE), "legacy safe bytes");
        expect(
            target.data.join(layout::PIX_DIR).join(layout::PIX_DB_FILE),
            "legacy redb bytes",
        );
        expect_dir(target.data.clone());
        expect_dir(target.cache.clone());
        expect_dir(target.data.join(layout::PIX_DIR));
        let marker = fs::read_to_string(target.data.join(layout::MIGRATED_FROM_FILE)).unwrap();
        assert!(marker.contains(&format!("source = {}", fx.legacy.join(".config").display())));
        assert!(marker.contains("toolkit_version = 0.5.0-test"));

        assert_eq!(legacy.len(), 2);
        assert!(!fx.legacy.join(".config").exists());
        assert!(!fx.legacy.join(layout::PIX_DB_FILE).exists());
        let aside = &legacy[0].to;
        assert!(
            aside
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".config.migrated-")
        );
        assert_eq!(
            fs::read_to_string(aside.join(layout::LEGACY_IDENTITY_FILE)).unwrap(),
            "legacy id bytes"
        );
        assert!(legacy[1].to.to_str().unwrap().contains("curvy-pix.redb.migrated-"));
        let staging_left = entries(&fx.layout().data.join(layout::NETWORKS_DIR))
            .into_iter()
            .any(|n| n.contains("migrating"));
        assert!(!staging_left);
    }

    #[test]
    fn a_failure_after_staging_removes_staging_and_restores_the_replaced_identity() {
        // Root ignores the read-only dir this test relies on.
        if user::is_root() {
            return;
        }
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::write(target.data.join(layout::IDENTITY_FILE), "fresh id").unwrap();
        let layout = fx.layout();
        let cache_networks = layout.cache.join(layout::NETWORKS_DIR);
        fs::create_dir_all(&cache_networks).unwrap();
        // The data staging is filled before the cache staging dir is created, and that creation fails.
        fs::set_permissions(&cache_networks, fs::Permissions::from_mode(0o500)).unwrap();
        let result = fx.run(false, true);
        fs::set_permissions(&cache_networks, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.unwrap_err().stage, Stage::Destination);
        assert_eq!(
            entries(&layout.data.join(layout::NETWORKS_DIR)),
            vec![NETWORK.to_string()]
        );
        assert_eq!(
            fs::read_to_string(target.data.join(layout::IDENTITY_FILE)).unwrap(),
            "fresh id"
        );
        assert!(entries(&cache_networks).is_empty());
        assert!(fx.legacy.join(".config").join(layout::LEGACY_IDENTITY_FILE).exists());
    }

    #[test]
    fn a_failed_cache_move_aside_puts_the_data_dir_back() {
        // Root ignores the read-only dir this test relies on.
        if user::is_root() {
            return;
        }
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::create_dir_all(&target.cache).unwrap();
        fs::write(target.data.join(layout::IDENTITY_FILE), "fresh id").unwrap();
        let cache_networks = fx.layout().cache.join(layout::NETWORKS_DIR);
        fs::set_permissions(&cache_networks, fs::Permissions::from_mode(0o500)).unwrap();
        let result = fx.run(false, true);
        fs::set_permissions(&cache_networks, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.unwrap_err().stage, Stage::Destination);
        assert_eq!(
            entries(&fx.layout().data.join(layout::NETWORKS_DIR)),
            vec![NETWORK.to_string()]
        );
        assert_eq!(
            fs::read_to_string(target.data.join(layout::IDENTITY_FILE)).unwrap(),
            "fresh id"
        );
        assert_eq!(entries(&cache_networks), vec![NETWORK.to_string()]);
    }

    #[test]
    fn a_failed_cache_publish_takes_the_data_dir_back() {
        let fx = fixture();
        fx.with_legacy(&["id", "safe"]);
        let legacy = Legacy::find(&fx.legacy).unwrap().unwrap();
        let plan = legacy.plan().unwrap();
        let networks = Networks::create(&fx.account, &layout::user_layout()).unwrap();
        stage(&networks, "staging", &plan, "marker", &fx.account).unwrap();
        // A non-empty dir cannot be renamed over.
        let blocker = fx.target().cache.join("occupied");
        fs::create_dir_all(&blocker).unwrap();

        let err = publish(&networks, "staging", NETWORK).unwrap_err();
        assert_eq!(err.stage, Stage::Destination);
        let layout = fx.layout();
        assert_eq!(
            entries(&layout.data.join(layout::NETWORKS_DIR)),
            vec!["staging".to_string()]
        );
        networks.remove("staging");
        assert!(entries(&layout.data.join(layout::NETWORKS_DIR)).is_empty());
        assert_eq!(
            entries(&layout.cache.join(layout::NETWORKS_DIR)),
            vec![NETWORK.to_string()]
        );
    }

    #[test]
    fn a_planted_symlink_in_the_destination_is_never_written_through() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        let victim = fx._tmp.path().join("victim");
        fs::write(&victim, "root-owned bytes").unwrap();
        let networks = fx.layout().data.join(layout::NETWORKS_DIR);
        fs::create_dir_all(&networks).unwrap();
        // The staging name is predictable to the second, so plant it for the next few seconds.
        let now = chrono::Utc::now();
        for offset in 0..5 {
            let ts = (now + chrono::Duration::seconds(offset)).format("%Y%m%d%H%M%S");
            std::os::unix::fs::symlink(&victim, networks.join(format!("{NETWORK}.migrating-{ts}"))).unwrap();
        }
        let err = fx.run(false, false).unwrap_err();
        assert_eq!(err.stage, Stage::Destination);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "root-owned bytes");
        assert!(fx.legacy.join(".config").join(layout::LEGACY_IDENTITY_FILE).exists());
    }

    #[test]
    fn a_planted_symlink_in_the_legacy_dir_is_never_read_through() {
        let fx = fixture();
        fx.with_legacy(&["pass"]);
        let secret = fx._tmp.path().join("secret");
        fs::write(&secret, "root-only bytes").unwrap();
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        std::os::unix::fs::symlink(&secret, config.join(layout::LEGACY_IDENTITY_FILE)).unwrap();
        assert_eq!(fx.run(false, false).unwrap_err().stage, Stage::Legacy);

        fs::remove_file(config.join(layout::LEGACY_IDENTITY_FILE)).unwrap();
        fx.with_legacy(&["id"]);
        std::os::unix::fs::symlink(&secret, fx.legacy.join(layout::PIX_DB_FILE)).unwrap();
        assert_eq!(fx.run(false, false).unwrap_err().stage, Stage::Legacy);
        assert!(entries(&fx.account.home).is_empty());
    }

    #[test]
    fn a_planted_hard_link_in_the_legacy_dir_is_refused() {
        let fx = fixture();
        fx.with_legacy(&["pass"]);
        let secret = fx._tmp.path().join("secret");
        fs::write(&secret, "root-only bytes").unwrap();
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        fs::hard_link(&secret, config.join(layout::LEGACY_IDENTITY_FILE)).unwrap();
        assert_eq!(fx.run(false, false).unwrap_err().stage, Stage::Legacy);
        assert!(entries(&fx.account.home).is_empty());
    }

    #[test]
    fn a_symlinked_legacy_config_dir_is_refused() {
        let fx = fixture();
        let elsewhere = fx._tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join(layout::LEGACY_IDENTITY_FILE), "planted").unwrap();
        std::os::unix::fs::symlink(&elsewhere, fx.legacy.join(layout::LEGACY_CONFIG_DIR)).unwrap();
        assert_eq!(fx.run(false, false).unwrap_err().stage, Stage::Legacy);
    }

    #[test]
    fn rerun_finds_nothing_and_a_restored_legacy_is_already_migrated() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe"]);
        let Outcome::Migrated { legacy, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Migrated");
        };
        assert!(matches!(
            fx.run(false, false).unwrap(),
            Outcome::NothingToMigrate { .. }
        ));
        fs::rename(&legacy[0].to, &legacy[0].from).unwrap();
        let Outcome::AlreadyMigrated { legacy, .. } = fx.run(true, false).unwrap() else {
            panic!("expected AlreadyMigrated");
        };
        assert!(legacy.is_empty(), "a dry run must touch nothing");
        assert!(fx.legacy.join(".config").exists());
    }

    #[test]
    fn already_migrated_finishes_moving_the_legacy_aside() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "redb"]);
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        let Outcome::Migrated { legacy, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Migrated");
        };
        // As if the run had stopped right after publishing.
        for m in &legacy {
            fs::rename(&m.to, &m.from).unwrap();
        }
        fs::set_permissions(&config, fs::Permissions::from_mode(0o775)).unwrap();

        let Outcome::AlreadyMigrated { legacy, .. } = fx.run(false, false).unwrap() else {
            panic!("expected AlreadyMigrated");
        };
        assert_eq!(legacy.len(), 2);
        assert!(!config.exists());
        assert!(!fx.legacy.join(layout::PIX_DB_FILE).exists());
        assert_eq!(mode(&legacy[0].to), layout::DIR_MODE);
    }

    #[test]
    fn a_symlinked_network_dir_is_never_hashed_through() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        let elsewhere = fx._tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join(layout::IDENTITY_FILE), "legacy id bytes").unwrap();
        let target = fx.target();
        fs::create_dir_all(target.data.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &target.data).unwrap();
        let Outcome::Conflict { mismatches, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Conflict");
        };
        assert_eq!(mismatches[0].difference, Difference::Missing);
    }

    #[test]
    fn a_dangling_symlink_at_the_network_dir_is_a_conflict_not_a_silent_replace() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        let target = fx.target();
        fs::create_dir_all(target.data.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(fx._tmp.path().join("nowhere"), &target.data).unwrap();
        let Outcome::Conflict { mismatches, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Conflict");
        };
        assert_eq!(mismatches[0].difference, Difference::Missing);
        assert!(
            fs::symlink_metadata(&target.data).is_ok(),
            "the symlink must stay in place"
        );
    }

    #[test]
    fn a_fifo_in_the_destination_reads_as_missing_without_hanging() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        let fifo = target.data.join(layout::IDENTITY_FILE);
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let Outcome::Conflict { mismatches, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Conflict");
        };
        assert_eq!(mismatches[0].difference, Difference::Missing);
    }

    #[test]
    fn conflict_names_missing_and_differing_files() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::write(target.data.join(layout::IDENTITY_FILE), "legacy id bytes").unwrap();
        let outcome = fx.run(false, false).unwrap();
        assert_eq!(outcome.exit_code(), exitcode::TEMPFAIL);
        let Outcome::Conflict { mismatches, .. } = outcome else {
            panic!("expected Conflict");
        };
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].difference, Difference::Missing);
        assert_eq!(mismatches[0].target, target.data.join(layout::PASSWORD_FILE));

        fs::write(target.data.join(layout::IDENTITY_FILE), "fresh id").unwrap();
        fs::write(target.data.join(layout::PASSWORD_FILE), "fresh pass").unwrap();
        let Outcome::Conflict { mismatches, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Conflict");
        };
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches.iter().all(|m| m.difference == Difference::Differs));
        assert!(fx.legacy.join(".config").exists(), "Conflict must touch nothing");
    }

    #[test]
    fn replace_moves_the_current_identity_aside_first() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::create_dir_all(&target.cache).unwrap();
        fs::write(target.data.join(layout::IDENTITY_FILE), "fresh id").unwrap();
        fs::write(target.cache.join(layout::SAFE_FILE), "fresh safe").unwrap();
        assert!(matches!(fx.run(false, true).unwrap(), Outcome::Migrated { .. }));
        assert_eq!(
            fs::read_to_string(target.data.join(layout::IDENTITY_FILE)).unwrap(),
            "legacy id bytes"
        );
        let layout = fx.layout();
        for base in [&layout.data, &layout.cache] {
            let replaced: Vec<String> = entries(&base.join(layout::NETWORKS_DIR))
                .into_iter()
                .filter(|n| n.starts_with("jura.replaced-"))
                .collect();
            assert_eq!(replaced.len(), 1, "{}", base.display());
        }
        let replaced = entries(&layout.data.join(layout::NETWORKS_DIR))
            .into_iter()
            .find(|n| n.starts_with("jura.replaced-"))
            .unwrap();
        let old_id = layout
            .data
            .join(layout::NETWORKS_DIR)
            .join(replaced)
            .join(layout::IDENTITY_FILE);
        assert_eq!(fs::read_to_string(old_id).unwrap(), "fresh id");
    }

    #[test]
    fn dry_run_touches_nothing() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe", "redb"]);
        let before = tree(fx._tmp.path());
        let outcome = fx.run(true, false).unwrap();
        let Outcome::WouldMigrate { files, .. } = outcome else {
            panic!("expected WouldMigrate");
        };
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].to, fx.target().data.join(layout::IDENTITY_FILE));
        assert_eq!(tree(fx._tmp.path()), before);
    }

    #[test]
    fn start_clean_moves_legacy_and_user_dirs_aside_but_keeps_the_redb() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "redb"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::create_dir_all(&target.cache).unwrap();
        let outcome = start_clean(&fx.legacy, Some((&fx.account, NETWORK))).unwrap();
        let Outcome::StartedClean { moved } = outcome else {
            panic!("expected StartedClean");
        };
        assert_eq!(moved.len(), 3);
        assert!(!fx.legacy.join(".config").exists());
        assert!(moved[0].to.to_str().unwrap().contains(".config."));
        assert!(moved[0].to.to_str().unwrap().ends_with(".bak"));
        assert!(fx.legacy.join(layout::PIX_DB_FILE).exists());
        assert!(!target.data.exists() && !target.cache.exists());
        let layout = fx.layout();
        for base in [&layout.data, &layout.cache] {
            let reset = entries(&base.join(layout::NETWORKS_DIR));
            assert_eq!(reset.len(), 1);
            assert!(reset[0].starts_with("jura.reset-"), "{}", reset[0]);
        }
        assert!(matches!(
            start_clean(&fx.legacy, Some((&fx.account, NETWORK))).unwrap(),
            Outcome::NothingToDo
        ));
    }

    #[test]
    fn the_start_clean_backup_is_locked_to_0700() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        fs::set_permissions(&config, fs::Permissions::from_mode(0o775)).unwrap();
        let Outcome::StartedClean { moved } = start_clean(&fx.legacy, None).unwrap() else {
            panic!("expected StartedClean");
        };
        assert_eq!(mode(&moved[0].to), layout::DIR_MODE);
        assert_eq!(fs::metadata(&moved[0].to).unwrap().uid(), uzers::get_effective_uid());
    }

    #[test]
    fn start_clean_without_a_user_only_touches_the_legacy_dir() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        let Outcome::StartedClean { moved } = start_clean(&fx.legacy, None).unwrap() else {
            panic!("expected StartedClean");
        };
        assert_eq!(moved.len(), 1);
        assert!(target.data.exists());
    }

    #[test]
    fn parents_of_a_bare_home_get_their_modes_and_the_owner() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        fx.run(false, false).unwrap();
        let layout = fx.layout();
        let own_dirs = [
            &layout.data,
            &layout.data.join(layout::NETWORKS_DIR),
            &layout.cache,
            &layout.cache.join(layout::NETWORKS_DIR),
        ];
        let expected = layout
            .parents
            .iter()
            .map(|(dir, m)| (dir, *m))
            .chain(own_dirs.into_iter().map(|dir| (dir, layout::DIR_MODE)));
        for (dir, m) in expected {
            assert_eq!(mode(dir), m, "{}", dir.display());
            assert_eq!(fs::metadata(dir).unwrap().uid(), fx.account.uid);
        }
    }

    /// Only `just test-root` reaches this: chown to a foreign uid needs root.
    #[test]
    fn as_root_everything_created_belongs_to_the_account() {
        if !user::is_root() {
            return;
        }
        let nobody = uzers::get_user_by_name("nobody").expect("a nobody account");
        let mut fx = fixture();
        fx.account.uid = nobody.uid();
        fx.account.gid = nobody.primary_group_id();
        fx.with_legacy(&["id", "pass", "safe", "redb"]);
        fx.run(false, false).unwrap();
        let layout = fx.layout();
        let mut created: Vec<PathBuf> = layout.parents.iter().map(|(dir, _)| dir.clone()).collect();
        for base in [&layout.data, &layout.cache] {
            created.extend(tree(base).into_iter().map(|rel| base.join(rel)));
            created.push(base.clone());
        }
        assert!(created.iter().any(|p| p.ends_with(layout::MIGRATED_FROM_FILE)));
        for path in created {
            let meta = fs::symlink_metadata(&path).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid()),
                (fx.account.uid, fx.account.gid),
                "{}",
                path.display()
            );
        }
    }

    fn migrated_config_aside(fx: &Fixture) -> PathBuf {
        let Outcome::Migrated { legacy, .. } = fx.run(false, false).unwrap() else {
            panic!("expected Migrated");
        };
        legacy[0].to.clone()
    }

    #[test]
    fn the_migrated_legacy_config_is_locked_to_0700() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        fs::set_permissions(&config, fs::Permissions::from_mode(0o775)).unwrap();
        let aside = migrated_config_aside(&fx);
        assert_eq!(mode(&aside), layout::DIR_MODE);
        assert_eq!(fs::metadata(&aside).unwrap().uid(), uzers::get_effective_uid());
    }

    /// Only `just test-root` reaches this: taking the dir from another owner needs root.
    #[test]
    fn as_root_the_migrated_legacy_config_belongs_to_root() {
        if !user::is_root() {
            return;
        }
        let nobody = uzers::get_user_by_name("nobody").expect("a nobody account");
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        let config = fx.legacy.join(layout::LEGACY_CONFIG_DIR);
        std::os::unix::fs::chown(&config, Some(nobody.uid()), Some(nobody.primary_group_id())).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o775)).unwrap();
        let aside = migrated_config_aside(&fx);
        let meta = fs::metadata(&aside).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (0, 0));
        assert_eq!(mode(&aside), layout::DIR_MODE);
    }

    fn with_leftovers(fx: &Fixture) {
        fs::create_dir_all(fx.legacy.join(".cache/x")).unwrap();
        fs::write(fx.legacy.join(".cache/x/y"), "cached").unwrap();
        fs::write(fx.legacy.join(".tmp123"), "tmp").unwrap();
    }

    fn secured_names(fx: &Fixture, legacy_uid: u32) -> Vec<String> {
        let home = nofollow::open_home(&fx.legacy).unwrap();
        let mut names: Vec<String> = secure_leftovers(&home, &fx.legacy, legacy_uid)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_migration_secures_the_legacy_owned_leftovers() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        with_leftovers(&fx);
        let names = secured_names(&fx, uzers::get_effective_uid());
        assert_eq!(names, vec![".cache", ".config", ".tmp123"]);
    }

    #[test]
    fn entries_owned_by_someone_else_are_left_alone() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        with_leftovers(&fx);
        fs::set_permissions(fx.legacy.join(".tmp123"), fs::Permissions::from_mode(0o664)).unwrap();
        let names = secured_names(&fx, uzers::get_effective_uid() + 1);
        assert_eq!(names, vec![".config"], "only the lock, no re-own");
        assert_eq!(mode(&fx.legacy.join(".tmp123")), Mode::from_bits_truncate(0o664));
    }

    #[test]
    fn a_leftover_config_dir_is_locked_to_0700_only_after_a_migration() {
        let fx = fixture();
        let open = Mode::from_bits_truncate(0o775);
        let leftovers = [".config-jura-dev", ".config.20260101000000.bak"].map(|name| fx.legacy.join(name));
        for dir in &leftovers {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(layout::LEGACY_IDENTITY_FILE), "other id").unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o775)).unwrap();
        }
        assert!(matches!(
            fx.run(false, false).unwrap(),
            Outcome::NothingToMigrate { .. }
        ));
        fx.with_legacy(&["id"]);
        let target = fx.target();
        fs::create_dir_all(&target.data).unwrap();
        fs::write(target.data.join(layout::IDENTITY_FILE), "fresh id").unwrap();
        assert!(matches!(fx.run(false, false).unwrap(), Outcome::Conflict { .. }));
        for dir in &leftovers {
            assert_eq!(mode(dir), open, "{} before a migration", dir.display());
        }

        assert!(matches!(fx.run(false, true).unwrap(), Outcome::Migrated { .. }));
        for dir in &leftovers {
            assert_eq!(mode(dir), layout::DIR_MODE, "{}", dir.display());
            assert_eq!(fs::metadata(dir).unwrap().uid(), uzers::get_effective_uid());
        }
    }

    #[test]
    fn a_planted_symlink_in_the_legacy_home_is_never_followed() {
        let fx = fixture();
        fx.with_legacy(&["id"]);
        let outside_dir = fx._tmp.path().join("outside_dir");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::set_permissions(&outside_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let outside_file = fx._tmp.path().join("outside_file");
        fs::write(&outside_file, "root-only bytes").unwrap();
        fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir_all(fx.legacy.join(".cache")).unwrap();
        let config_link = fx.legacy.join(".config-evil");
        let cache_link = fx.legacy.join(".cache/link");
        std::os::unix::fs::symlink(&outside_dir, &config_link).unwrap();
        std::os::unix::fs::symlink(&outside_file, &cache_link).unwrap();

        assert!(matches!(fx.run(false, false).unwrap(), Outcome::Migrated { .. }));
        assert_eq!(mode(&outside_dir), Mode::from_bits_truncate(0o755));
        assert_eq!(mode(&outside_file), Mode::from_bits_truncate(0o644));
        assert_eq!(fs::read_to_string(&outside_file).unwrap(), "root-only bytes");
        for link in [&config_link, &cache_link] {
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
    }

    /// Only `just test-root` reaches this: handing files to `nobody` and back needs root.
    #[test]
    fn as_root_leftovers_belong_to_root_and_foreign_entries_keep_their_owner() {
        if !user::is_root() {
            return;
        }
        let nobody = uzers::get_user_by_name("nobody").expect("a nobody account");
        let (uid, gid) = (nobody.uid(), nobody.primary_group_id());
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        with_leftovers(&fx);
        let outside_file = fx._tmp.path().join("outside_file");
        fs::write(&outside_file, "theirs").unwrap();
        // Stands in for the updater's root:gnosisvpn files.
        let updater = fx.legacy.join("updater");
        fs::write(&updater, "root's").unwrap();
        std::os::unix::fs::chown(&updater, Some(0), Some(gid)).unwrap();
        let link = fx.legacy.join(".cache/link");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        let legacy_owned = [".config", ".cache", ".cache/x", ".cache/x/y", ".tmp123"].map(|rel| fx.legacy.join(rel));
        for path in legacy_owned.iter().chain([&outside_file]) {
            std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
        }
        std::os::unix::fs::lchown(&link, Some(uid), Some(gid)).unwrap();

        assert!(matches!(fx.run(false, false).unwrap(), Outcome::Migrated { .. }));
        let owner = |path: &Path| {
            let meta = fs::symlink_metadata(path).unwrap();
            (meta.uid(), meta.gid())
        };
        for path in legacy_owned.iter().skip(1).chain([&link]) {
            assert_eq!(owner(path), (0, 0), "{}", path.display());
        }
        assert_eq!(owner(&updater), (0, gid));
        assert_eq!(owner(&outside_file), (uid, gid));
    }

    #[test]
    fn copies_across_filesystems() {
        // The source stays in the temp dir (a tmpfs on many hosts); the home lives next to the build output.
        let tmp = tempfile::tempdir().unwrap();
        let home = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("target")
            .join(format!("migrate-xfs-home-{}", std::process::id()));
        let fx = fixture_in(tmp.path().join("legacy"), home.clone(), tmp);
        fx.with_legacy(&["id", "pass", "safe"]);
        let source_dev = fs::metadata(&fx.legacy).unwrap().dev();
        let home_dev = fs::metadata(&home).unwrap().dev();
        let outcome = fx.run(false, false);
        let cleanup = fs::remove_dir_all(&home);
        assert!(matches!(outcome.unwrap(), Outcome::Migrated { .. }));
        cleanup.unwrap();
        if source_dev == home_dev {
            eprintln!("note: temp dir and target dir share a filesystem; the copy was not cross-device");
        }
    }

    #[test]
    fn exit_codes_follow_the_installer_contract() {
        assert_eq!(Stage::Privileges.exit_code(), 77);
        assert_eq!(Stage::User.exit_code(), 64);
        assert_eq!(Stage::Network.exit_code(), 65);
        assert_eq!(Stage::Legacy.exit_code(), 66);
        assert_eq!(Stage::Destination.exit_code(), 73);
        assert_eq!(Stage::Upload.exit_code(), 70);
        let conflict = Outcome::Conflict {
            network: NETWORK.into(),
            user: "u".into(),
            mismatches: vec![],
        };
        assert_eq!(conflict.exit_code(), 75);
        for ok in [
            Outcome::NothingToDo,
            Outcome::NothingToMigrate {
                legacy_home: PathBuf::new(),
            },
            Outcome::AlreadyMigrated {
                network: NETWORK.into(),
                user: "u".into(),
                legacy: vec![],
            },
            Outcome::WouldMigrate {
                network: NETWORK.into(),
                user: "u".into(),
                files: vec![],
            },
            Outcome::Uploaded { uuid: "x".into() },
        ] {
            assert_eq!(ok.exit_code(), 0, "{ok}");
        }
    }

    #[test]
    fn json_outcomes_are_tagged() {
        let failed = Outcome::from_failure(Failure::new(Stage::Network, "bad link"));
        let json = serde_json::to_value(MigrateResult { outcome: &failed }).unwrap();
        assert_eq!(json["outcome"]["type"], "Failed");
        assert_eq!(json["outcome"]["stage"]["type"], "Network");
        assert_eq!(json["outcome"]["error"], "bad link");
        let conflict = Outcome::Conflict {
            network: NETWORK.into(),
            user: "u".into(),
            mismatches: vec![Mismatch {
                legacy: "/l".into(),
                target: "/t".into(),
                difference: Difference::Missing,
            }],
        };
        let json = serde_json::to_value(&conflict).unwrap();
        assert_eq!(json["mismatches"][0]["difference"]["type"], "Missing");
    }

    #[test]
    fn aside_names_never_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = nofollow::open_home(tmp.path()).unwrap();
        fs::create_dir(tmp.path().join("x.bak")).unwrap();
        assert_eq!(unique_name(&dir, "x.bak".into()).unwrap(), "x.bak.1");
        fs::create_dir(tmp.path().join("x.bak.1")).unwrap();
        assert_eq!(unique_name(&dir, "x.bak".into()).unwrap(), "x.bak.2");
    }
}
