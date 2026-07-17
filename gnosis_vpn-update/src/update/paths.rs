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
