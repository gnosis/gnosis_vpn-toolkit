//! Legacy and per-user paths; the per-user rules duplicate the client's GNO-810 contract so the toolkit needs no gnosis_vpn-lib.

use std::path::{Path, PathBuf};

use rustix::fs::Mode;

pub const IDENTITY_FILE: &str = "hopr.id";
pub const PASSWORD_FILE: &str = "hopr.pass";
pub const SAFE_FILE: &str = "hopr.safe";
pub const PIX_DIR: &str = "pix";
pub const PIX_DB_FILE: &str = "curvy-pix.redb";
pub const MIGRATED_FROM_FILE: &str = "migrated-from";
pub const DIR_MODE: Mode = Mode::from_bits_truncate(0o700);
pub const FILE_MODE: Mode = Mode::from_bits_truncate(0o600);
// Shared parents (XDG, ~/Library subdirs) get the mode the OS itself would give them.
pub(crate) const SHARED_DIR_MODE: Mode = Mode::from_bits_truncate(0o755);

/// The app's tauri identifier, which names its per-user dirs.
pub const APP_ID: &str = "com.gnosisvpn.gnosisvpnclient";

pub const LEGACY_CONFIG_DIR: &str = ".config";
pub const LEGACY_IDENTITY_FILE: &str = "gnosisvpn-hopr.id";
pub const LEGACY_PASSWORD_FILE: &str = "gnosisvpn-hopr.pass";
pub const LEGACY_SAFE_FILE: &str = "gnosisvpn-hopr.safe";

#[cfg(target_os = "macos")]
pub fn legacy_home() -> PathBuf {
    PathBuf::from("/Library/Application Support/GnosisVPN")
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub fn migrate_log() -> PathBuf {
    PathBuf::from("/var/log/gnosisvpn/migrate.log")
}

/// What `upload-log` bundles, in order; the migrate log is always reported.
pub struct LogSources {
    pub migrate: PathBuf,
    pub extra: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
pub fn log_sources() -> LogSources {
    let installer = PathBuf::from("/Library/Logs/GnosisVPN/installer");
    LogSources {
        migrate: migrate_log(),
        extra: vec![
            installer.join("installer.log"),
            installer.join("postinstall.log"),
            PathBuf::from("/Library/Logs/GnosisVPN/updates.log"),
        ],
    }
}

#[cfg(target_os = "linux")]
pub fn log_sources() -> LogSources {
    LogSources {
        migrate: migrate_log(),
        extra: Vec::new(),
    }
}

/// The per-user roots plus the parents a bare account may lack, all relative to the home dir, with the mode each gets when created.
pub struct UserLayout {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub parents: Vec<(PathBuf, Mode)>,
}

#[cfg(target_os = "macos")]
pub fn user_layout() -> UserLayout {
    // An account that never logged in may lack ~/Library or its subdirs.
    UserLayout {
        data: PathBuf::from("Library/Application Support/GnosisVPN"),
        cache: PathBuf::from("Library/Caches/GnosisVPN"),
        parents: vec![
            (PathBuf::from("Library"), DIR_MODE),
            (PathBuf::from("Library/Application Support"), SHARED_DIR_MODE),
            (PathBuf::from("Library/Caches"), SHARED_DIR_MODE),
        ],
    }
}

#[cfg(target_os = "linux")]
pub fn user_layout() -> UserLayout {
    UserLayout {
        data: PathBuf::from(".local/share/gnosisvpn"),
        cache: PathBuf::from(".cache/gnosisvpn"),
        parents: vec![
            (PathBuf::from(".local"), SHARED_DIR_MODE),
            (PathBuf::from(".local/share"), SHARED_DIR_MODE),
            (PathBuf::from(".cache"), SHARED_DIR_MODE),
        ],
    }
}

pub const NETWORKS_DIR: &str = "networks";

pub fn network_dir(base: &Path, network: &str) -> PathBuf {
    base.join(NETWORKS_DIR).join(network)
}
