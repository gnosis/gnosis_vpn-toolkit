//! The migration: enumerate, compare, copy into a staging dir, verify, rename into place. Nothing is ever deleted.

use std::fmt;
use std::fs::{self, File, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use exitcode::ExitCode;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::layout::{self, UserLayout};
use crate::user::{self, Account};

/// Where a run failed; each stage has one exit code, which is what the installer scripts key on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
            Outcome::AlreadyMigrated { network, user } => {
                write!(
                    f,
                    "Identity already migrated for user {user} on network {network}; nothing to do"
                )
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

pub fn legacy_identity_exists(legacy_home: &Path) -> bool {
    Legacy::find(legacy_home).is_some()
}

pub fn migrate_identity(req: &IdentityRequest<'_>) -> Result<Outcome, Failure> {
    let Some(legacy) = Legacy::find(req.legacy_home) else {
        tracing::info!(legacy_home = %req.legacy_home.display(), "no legacy identity found");
        return Ok(Outcome::NothingToMigrate {
            legacy_home: req.legacy_home.to_path_buf(),
        });
    };
    let plan = legacy.plan()?;
    let layout = layout::user_layout(&req.account.home);
    let target = Target::new(&layout, req.network);
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

    if target.exists() {
        let mismatches = target.compare(&plan)?;
        if mismatches.is_empty() {
            tracing::info!(target = %target.data.display(), "destination already holds the legacy identity");
            return Ok(Outcome::AlreadyMigrated { network, user });
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
    if target.exists() {
        target.move_aside("replaced", &ts)?;
    }
    create_user_dirs(&layout, req.account)?;
    let staging = Target::new(&layout, &format!("{}.migrating-{ts}", req.network));
    if let Err(e) = stage_files(&plan, &staging, req.account) {
        tracing::warn!(error = %e.error, "staging failed, removing the staging dirs");
        staging.remove();
        return Err(e);
    }
    rename(&staging.data, &target.data)?;
    rename(&staging.cache, &target.cache)?;
    let legacy_moves = legacy.move_aside(&ts)?;
    write_marker(&target.data.join(layout::MIGRATED_FROM_FILE), &legacy, req)?;
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
    let config_dir = legacy_home.join(layout::LEGACY_CONFIG_DIR);
    if config_dir.exists() {
        let aside = unique_aside(config_dir.with_file_name(format!("{}.{ts}.bak", layout::LEGACY_CONFIG_DIR)));
        fs::rename(&config_dir, &aside).map_err(|e| Failure::new(Stage::Legacy, with_path(&config_dir, &e)))?;
        tracing::info!(from = %config_dir.display(), to = %aside.display(), "legacy identity moved aside");
        moved.push(Move {
            from: config_dir,
            to: aside,
        });
    }
    if let Some((account, network)) = user_dirs {
        let layout = layout::user_layout(&account.home);
        moved.extend(Target::new(&layout, network).move_aside("reset", &ts)?);
    }
    if moved.is_empty() {
        return Ok(Outcome::NothingToDo);
    }
    Ok(Outcome::StartedClean { moved })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Data,
    Cache,
}

struct Planned {
    from: PathBuf,
    place: Place,
    rel: PathBuf,
    sha256: [u8; 32],
}

/// The legacy files: `.config/gnosisvpn-hopr.{id,pass,safe}` and the Curvy PIX db.
struct Legacy {
    config_dir: PathBuf,
    redb: Option<PathBuf>,
    sources: Vec<(PathBuf, Place, PathBuf)>,
}

impl Legacy {
    fn find(home: &Path) -> Option<Self> {
        let config_dir = home.join(layout::LEGACY_CONFIG_DIR);
        let id = config_dir.join(layout::LEGACY_IDENTITY_FILE);
        if !id.is_file() {
            return None;
        }
        let present = |path: PathBuf| path.is_file().then_some(path);
        let mut sources = vec![(id, Place::Data, PathBuf::from(layout::IDENTITY_FILE))];
        if let Some(pass) = present(config_dir.join(layout::LEGACY_PASSWORD_FILE)) {
            sources.push((pass, Place::Data, PathBuf::from(layout::PASSWORD_FILE)));
        }
        if let Some(safe) = present(config_dir.join(layout::LEGACY_SAFE_FILE)) {
            sources.push((safe, Place::Cache, PathBuf::from(layout::SAFE_FILE)));
        }
        let redb = present(home.join(layout::PIX_DB_FILE));
        if let Some(redb) = &redb {
            let rel = Path::new(layout::PIX_DIR).join(layout::PIX_DB_FILE);
            sources.push((redb.clone(), Place::Data, rel));
        }
        Some(Self {
            config_dir,
            redb,
            sources,
        })
    }

    /// Hashing doubles as the readability check, so an unreadable legacy file fails before anything is created.
    fn plan(&self) -> Result<Vec<Planned>, Failure> {
        self.sources
            .iter()
            .map(|(from, place, rel)| {
                let sha256 = sha256(from).map_err(|e| Failure::new(Stage::Legacy, with_path(from, &e)))?;
                Ok(Planned {
                    from: from.clone(),
                    place: *place,
                    rel: rel.clone(),
                    sha256,
                })
            })
            .collect()
    }

    fn move_aside(&self, ts: &str) -> Result<Vec<Move>, Failure> {
        let mut moved = Vec::new();
        let config_aside = unique_aside(
            self.config_dir
                .with_file_name(format!("{}.migrated-{ts}", layout::LEGACY_CONFIG_DIR)),
        );
        moved.push(rename_aside(&self.config_dir, config_aside)?);
        if let Some(redb) = &self.redb {
            let aside = unique_aside(redb.with_file_name(format!("{}.migrated-{ts}", layout::PIX_DB_FILE)));
            moved.push(rename_aside(redb, aside)?);
        }
        Ok(moved)
    }
}

fn rename_aside(from: &Path, to: PathBuf) -> Result<Move, Failure> {
    fs::rename(from, &to).map_err(|e| Failure::new(Stage::Legacy, with_path(from, &e)))?;
    tracing::info!(from = %from.display(), to = %to.display(), "legacy files moved aside");
    Ok(Move {
        from: from.to_path_buf(),
        to,
    })
}

/// The per-network dir pair; also used for the staging pair and for moving either aside.
struct Target {
    data: PathBuf,
    cache: PathBuf,
}

impl Target {
    fn new(layout: &UserLayout, name: &str) -> Self {
        Self {
            data: layout::network_dir(&layout.data, name),
            cache: layout::network_dir(&layout.cache, name),
        }
    }

    fn exists(&self) -> bool {
        self.data.exists() || self.cache.exists()
    }

    fn path(&self, planned: &Planned) -> PathBuf {
        match planned.place {
            Place::Data => self.data.join(&planned.rel),
            Place::Cache => self.cache.join(&planned.rel),
        }
    }

    fn compare(&self, plan: &[Planned]) -> Result<Vec<Mismatch>, Failure> {
        let mut mismatches = Vec::new();
        for planned in plan {
            let target = self.path(planned);
            if !target.is_file() {
                mismatches.push(Mismatch {
                    legacy: planned.from.clone(),
                    target,
                    difference: Difference::Missing,
                });
                continue;
            }
            let hash = sha256(&target).map_err(|e| Failure::new(Stage::Destination, with_path(&target, &e)))?;
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

    /// Renames whichever of the pair exists to a `<name>.<reason>-<ts>` sibling.
    fn move_aside(&self, reason: &str, ts: &str) -> Result<Vec<Move>, Failure> {
        let mut moved = Vec::new();
        for dir in [&self.data, &self.cache] {
            if !dir.exists() {
                continue;
            }
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("network");
            let aside = unique_aside(dir.with_file_name(format!("{name}.{reason}-{ts}")));
            fs::rename(dir, &aside).map_err(|e| Failure::new(Stage::Destination, with_path(dir, &e)))?;
            tracing::info!(from = %dir.display(), to = %aside.display(), "per-user network dir moved aside");
            moved.push(Move {
                from: dir.clone(),
                to: aside,
            });
        }
        Ok(moved)
    }

    fn remove(&self) {
        let _ = fs::remove_dir_all(&self.data);
        let _ = fs::remove_dir_all(&self.cache);
    }
}

fn create_user_dirs(layout: &UserLayout, account: &Account) -> Result<(), Failure> {
    for (dir, mode) in &layout.parents {
        ensure_dir(dir, *mode, account)?;
    }
    for base in [&layout.data, &layout.cache] {
        ensure_dir(base, layout::DIR_MODE, account)?;
        ensure_dir(&base.join("networks"), layout::DIR_MODE, account)?;
    }
    Ok(())
}

/// Data files first, then cache, so an injected cache failure still exercises the data cleanup.
fn stage_files(plan: &[Planned], staging: &Target, account: &Account) -> Result<(), Failure> {
    ensure_dir(&staging.data, layout::DIR_MODE, account)?;
    for planned in plan.iter().filter(|p| p.place == Place::Data) {
        copy_verified(planned, &staging.path(planned), account)?;
    }
    ensure_dir(&staging.cache, layout::DIR_MODE, account)?;
    for planned in plan.iter().filter(|p| p.place == Place::Cache) {
        copy_verified(planned, &staging.path(planned), account)?;
    }
    Ok(())
}

/// Plain copy (rename cannot cross from /var/lib to /home), fsync, own, 0600, then hash against the source.
fn copy_verified(planned: &Planned, to: &Path, account: &Account) -> Result<(), Failure> {
    let dest = |e: io::Error| Failure::new(Stage::Destination, with_path(to, &e));
    if let Some(parent) = to.parent() {
        ensure_dir(parent, layout::DIR_MODE, account)?;
    }
    fs::copy(&planned.from, to).map_err(dest)?;
    File::open(to).and_then(|f| f.sync_all()).map_err(dest)?;
    fs::set_permissions(to, Permissions::from_mode(layout::FILE_MODE)).map_err(dest)?;
    user::chown(to, account).map_err(dest)?;
    let copied = sha256(to).map_err(dest)?;
    if copied != planned.sha256 {
        return Err(Failure::new(
            Stage::Destination,
            format!("{} differs from {} after copy", to.display(), planned.from.display()),
        ));
    }
    tracing::info!(from = %planned.from.display(), to = %to.display(), "copied and verified");
    Ok(())
}

fn ensure_dir(path: &Path, mode: u32, account: &Account) -> Result<(), Failure> {
    if path.is_dir() {
        return Ok(());
    }
    let dest = |e: io::Error| Failure::new(Stage::Destination, with_path(path, &e));
    fs::create_dir(path).map_err(dest)?;
    fs::set_permissions(path, Permissions::from_mode(mode)).map_err(dest)?;
    user::chown(path, account).map_err(dest)?;
    tracing::info!(path = %path.display(), mode = format_args!("{mode:o}"), "directory created");
    Ok(())
}

fn rename(from: &Path, to: &Path) -> Result<(), Failure> {
    fs::rename(from, to).map_err(|e| {
        Failure::new(
            Stage::Destination,
            format!("{} -> {}: {e}", from.display(), to.display()),
        )
    })?;
    tracing::info!(from = %from.display(), to = %to.display(), "renamed into place");
    Ok(())
}

fn write_marker(path: &Path, legacy: &Legacy, req: &IdentityRequest<'_>) -> Result<(), Failure> {
    let dest = |e: io::Error| Failure::new(Stage::Destination, with_path(path, &e));
    let content = format!(
        "source = {}\nmigrated_at = {}\ntoolkit_version = {}\n",
        legacy.config_dir.display(),
        chrono::Utc::now().to_rfc3339(),
        req.toolkit_version
    );
    fs::write(path, content).map_err(dest)?;
    fs::set_permissions(path, Permissions::from_mode(layout::FILE_MODE)).map_err(dest)?;
    user::chown(path, req.account).map_err(dest)
}

fn sha256(path: &Path) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().into())
}

fn with_path(path: &Path, e: &io::Error) -> String {
    format!("{}: {e}", path.display())
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d%H%M%S").to_string()
}

/// Two runs within one second must not rename onto each other.
fn unique_aside(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let base = candidate.to_string_lossy().into_owned();
    (1..)
        .map(|n| PathBuf::from(format!("{base}.{n}")))
        .find(|p| !p.exists())
        .unwrap_or(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

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
        // macOS has no XDG parents to create; the fixture plays the account's existing ~/Library.
        if cfg!(target_os = "macos") {
            fs::create_dir_all(home.join("Library/Application Support")).unwrap();
            fs::create_dir_all(home.join("Library/Caches")).unwrap();
        }
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

        fn layout(&self) -> UserLayout {
            layout::user_layout(&self.account.home)
        }

        fn target(&self) -> Target {
            Target::new(&self.layout(), NETWORK)
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

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
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
        assert!(entries(&fx.account.home).is_empty() || cfg!(target_os = "macos"));
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
        expect(target.data.join(layout::IDENTITY_FILE), "legacy id bytes");
        expect(target.data.join(layout::PASSWORD_FILE), "legacy pass bytes");
        expect(target.cache.join(layout::SAFE_FILE), "legacy safe bytes");
        expect(
            target.data.join(layout::PIX_DIR).join(layout::PIX_DB_FILE),
            "legacy redb bytes",
        );
        for dir in [&target.data, &target.cache, &target.data.join(layout::PIX_DIR)] {
            assert_eq!(mode(dir), layout::DIR_MODE, "{}", dir.display());
        }
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
        let staging_left = entries(&fx.layout().data.join("networks"))
            .into_iter()
            .any(|n| n.contains("migrating"));
        assert!(!staging_left);
    }

    #[test]
    fn a_failure_mid_copy_leaves_no_destination_or_staging_behind() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass", "safe"]);
        let layout = fx.layout();
        fs::create_dir_all(&layout.cache).unwrap();
        // A file where the cache `networks/` dir must go fails after the data files were already staged.
        fs::write(layout.cache.join("networks"), "blocker").unwrap();
        let err = fx.run(false, false).unwrap_err();
        assert_eq!(err.stage, Stage::Destination);
        assert!(!fx.target().data.exists());
        assert!(entries(&layout.data.join("networks")).is_empty());
        assert!(fx.legacy.join(".config").join(layout::LEGACY_IDENTITY_FILE).exists());
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
        assert!(matches!(fx.run(false, false).unwrap(), Outcome::AlreadyMigrated { .. }));
        assert!(fx.legacy.join(".config").exists(), "AlreadyMigrated must touch nothing");
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
            let replaced: Vec<String> = entries(&base.join("networks"))
                .into_iter()
                .filter(|n| n.starts_with("jura.replaced-"))
                .collect();
            assert_eq!(replaced.len(), 1, "{}", base.display());
        }
        let replaced = entries(&layout.data.join("networks"))
            .into_iter()
            .find(|n| n.starts_with("jura.replaced-"))
            .unwrap();
        let old_id = layout.data.join("networks").join(replaced).join(layout::IDENTITY_FILE);
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
            let reset = entries(&base.join("networks"));
            assert_eq!(reset.len(), 1);
            assert!(reset[0].starts_with("jura.reset-"), "{}", reset[0]);
        }
        assert!(matches!(
            start_clean(&fx.legacy, Some((&fx.account, NETWORK))).unwrap(),
            Outcome::NothingToDo
        ));
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn xdg_parents_of_a_bare_home_get_the_shell_modes_and_the_owner() {
        let fx = fixture();
        fx.with_legacy(&["id", "pass"]);
        fx.run(false, false).unwrap();
        let home = &fx.account.home;
        for (dir, expected) in [
            (home.join(".local"), 0o755),
            (home.join(".local/share"), 0o755),
            (home.join(".cache"), 0o755),
            (home.join(".local/share/gnosisvpn"), 0o700),
            (home.join(".local/share/gnosisvpn/networks"), 0o700),
            (home.join(".cache/gnosisvpn"), 0o700),
            (home.join(".cache/gnosisvpn/networks"), 0o700),
        ] {
            assert_eq!(mode(&dir), expected, "{}", dir.display());
            assert_eq!(fs::metadata(&dir).unwrap().uid(), fx.account.uid);
        }
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
        assert_eq!(json["outcome"]["stage"], "Network");
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
        let taken = tmp.path().join("x.bak");
        fs::create_dir(&taken).unwrap();
        assert_eq!(unique_aside(taken.clone()), tmp.path().join("x.bak.1"));
        fs::create_dir(tmp.path().join("x.bak.1")).unwrap();
        assert_eq!(unique_aside(taken), tmp.path().join("x.bak.2"));
    }
}
