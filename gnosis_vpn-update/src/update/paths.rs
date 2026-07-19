//! Hardcoded, root-owned filesystem locations used by the update engine.
//!
//! Centralized and never caller-influenced, so an untrusted invocation cannot
//! redirect where artifacts land or where the audit log + attempt-state file
//! live.

use std::path::PathBuf;

fn base_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/GnosisVPN")
}

pub fn download_dir() -> PathBuf {
    base_dir().join("updates")
}

pub fn attempt_state_path() -> PathBuf {
    base_dir().join("last_update_attempt.json")
}

pub fn audit_log_path() -> PathBuf {
    PathBuf::from("/var/log/gnosisvpn/updates.log")
}

/// Version file written by the client installer (both the macOS `.pkg` and the
/// Linux packages write the installed package version here).
pub fn installed_version_path() -> PathBuf {
    PathBuf::from("/etc/gnosisvpn/version.txt")
}

/// Active client config, maintained by the installer as a symlink to the
/// chosen network's config (e.g. `rotsee.toml`).
pub fn config_symlink_path() -> PathBuf {
    PathBuf::from("/etc/gnosisvpn/config.toml")
}

/// Directory where the GUI installer's choice packages record the user's
/// selections as one-line shell fragments.
fn installer_choice_dir() -> PathBuf {
    PathBuf::from("/Library/Logs/GnosisVPN/installer")
}

/// Recorded HOPR network selection (`INSTALLER_CHOICE_NETWORK="…"`).
pub fn network_choice_path() -> PathBuf {
    installer_choice_dir().join("network_choice")
}

/// Recorded log level selection (`INSTALLER_CHOICE_LOGLEVEL="…"`).
pub fn loglevel_choice_path() -> PathBuf {
    installer_choice_dir().join("loglevel_choice")
}
