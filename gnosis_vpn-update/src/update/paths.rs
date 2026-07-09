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
