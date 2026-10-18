//! Root-owned locations of the install engine, never caller-influenced so an untrusted invocation cannot redirect them.
//! The engine only runs on macOS (`install_platform` is a stub on Linux), so these are macOS locations without a platform split.

use std::path::{Path, PathBuf};

const STATE_DIR: &str = "/Library/Application Support/GnosisVPN";
const CACHE_DIR: &str = "/Library/Caches/GnosisVPN";
const LOG_DIR: &str = "/Library/Logs/GnosisVPN";

/// Downloaded packages; deleted after a successful install.
pub fn download_dir() -> PathBuf {
    Path::new(CACHE_DIR).join("updates")
}

pub fn attempt_state_path() -> PathBuf {
    Path::new(STATE_DIR).join("last_update_attempt.json")
}

/// One line per finished update, rotated by newsyslog like the service log.
pub fn audit_log_path() -> PathBuf {
    Path::new(LOG_DIR).join("updates.log")
}

pub use gnosis_vpn_toolkit_common::package_version::installed_version_path;

/// Active client config, maintained by the installer as a symlink to the chosen network's config (e.g. `rotsee.toml`).
pub fn config_symlink_path() -> PathBuf {
    PathBuf::from("/etc/gnosisvpn/config.toml")
}

/// Where the GUI installer's choice packages record the user's selections as one-line shell fragments.
fn installer_choice_dir() -> PathBuf {
    Path::new(STATE_DIR).join("installer")
}

/// Recorded HOPR network selection (`INSTALLER_CHOICE_NETWORK="…"`).
pub fn network_choice_path() -> PathBuf {
    installer_choice_dir().join("network_choice")
}

/// Recorded log level selection (`INSTALLER_CHOICE_LOGLEVEL="…"`).
pub fn loglevel_choice_path() -> PathBuf {
    installer_choice_dir().join("loglevel_choice")
}

/// The loader app polls this for `updating <pid>` and `done`.
pub fn update_status_path() -> PathBuf {
    installer_choice_dir().join("update_status")
}
