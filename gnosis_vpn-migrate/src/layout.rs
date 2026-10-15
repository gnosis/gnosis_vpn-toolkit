//! Legacy and per-user paths; the per-user rules duplicate the client's GNO-810 contract so the toolkit needs no gnosis_vpn-lib.

use std::path::{Path, PathBuf};

pub const IDENTITY_FILE: &str = "hopr.id";
pub const PASSWORD_FILE: &str = "hopr.pass";
pub const SAFE_FILE: &str = "hopr.safe";
pub const PIX_DIR: &str = "pix";
pub const PIX_DB_FILE: &str = "curvy-pix.redb";
pub const MIGRATED_FROM_FILE: &str = "migrated-from";
pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

pub const LEGACY_CONFIG_DIR: &str = ".config";
pub const LEGACY_IDENTITY_FILE: &str = "gnosisvpn-hopr.id";
pub const LEGACY_PASSWORD_FILE: &str = "gnosisvpn-hopr.pass";
pub const LEGACY_SAFE_FILE: &str = "gnosisvpn-hopr.safe";

#[cfg(target_os = "macos")]
pub fn legacy_home() -> PathBuf {
    PathBuf::from("/Library/Application Support/GnosisVPN")
}

#[cfg(not(target_os = "macos"))]
pub fn legacy_home() -> PathBuf {
    PathBuf::from("/var/lib/gnosisvpn")
}

/// The installer's network choice: a symlink to `config-<network>.toml` (Linux) or `<network>.toml` (macOS).
pub fn config_link() -> PathBuf {
    PathBuf::from("/etc/gnosisvpn/config.toml")
}

#[cfg(target_os = "macos")]
pub fn migrate_log() -> PathBuf {
    PathBuf::from("/Library/Logs/GnosisVPN/installer/migrate.log")
}

#[cfg(not(target_os = "macos"))]
pub fn migrate_log() -> PathBuf {
    PathBuf::from("/var/log/gnosisvpn/migrate.log")
}

/// Log files `upload-log` bundles, in bundle order; the migrate logs come first and are always reported.
pub struct LogSources {
    pub migrate: Vec<PathBuf>,
    pub extra: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
pub fn log_sources() -> LogSources {
    let installer = PathBuf::from("/Library/Logs/GnosisVPN/installer");
    LogSources {
        migrate: vec![migrate_log()],
        extra: vec![
            installer.join("installer.log"),
            installer.join("postinstall.log"),
            PathBuf::from("/var/log/gnosisvpn/updates.log"),
        ],
    }
}

#[cfg(not(target_os = "macos"))]
pub fn log_sources() -> LogSources {
    // rpm upgrades delete /var/log/gnosisvpn after the migration, so the installer copies the log next to the legacy data.
    LogSources {
        migrate: vec![migrate_log(), legacy_home().join("migrate.log")],
        extra: vec![PathBuf::from("/var/log/gnosisvpn/updates.log")],
    }
}

/// The per-user roots plus the parents a bare account may lack, with the mode each gets when created.
pub struct UserLayout {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub parents: Vec<(PathBuf, u32)>,
}

#[cfg(target_os = "macos")]
pub fn user_layout(home: &Path) -> UserLayout {
    UserLayout {
        data: home.join("Library/Application Support/GnosisVPN"),
        cache: home.join("Library/Caches/GnosisVPN"),
        parents: Vec::new(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn user_layout(home: &Path) -> UserLayout {
    // XDG parents are shared with every other program, so they get the shell's usual 0755.
    UserLayout {
        data: home.join(".local/share/gnosisvpn"),
        cache: home.join(".cache/gnosisvpn"),
        parents: vec![
            (home.join(".local"), 0o755),
            (home.join(".local/share"), 0o755),
            (home.join(".cache"), 0o755),
        ],
    }
}

pub fn network_dir(base: &Path, network: &str) -> PathBuf {
    base.join("networks").join(network)
}
