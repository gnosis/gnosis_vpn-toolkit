//! The installed client package version, from the file the installer writes.

use std::path::{Path, PathBuf};

/// Both the macOS `.pkg` and the Linux packages write the installed version here.
pub fn installed_version_path() -> PathBuf {
    PathBuf::from("/etc/gnosisvpn/version.txt")
}

/// Missing or empty is an error; the caller decides whether that is fatal (updater) or reportable (`version`).
pub fn read_installed_version(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read installed version from {}: {e}", path.display()))?;
    let version = raw.trim();
    if version.is_empty() {
        return Err(format!("installed version file {} is empty", path.display()));
    }
    Ok(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_installed_version_trims_and_rejects_missing_or_empty() {
        let dir = std::env::temp_dir().join(format!("toolkit-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("version.txt");
        std::fs::write(&path, "0.91.1\n").unwrap();
        assert_eq!(read_installed_version(&path).unwrap(), "0.91.1");
        std::fs::write(&path, "  \n").unwrap();
        assert!(read_installed_version(&path).unwrap_err().contains("empty"));
        std::fs::remove_file(&path).unwrap();
        assert!(read_installed_version(&path).unwrap_err().contains("cannot read"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
