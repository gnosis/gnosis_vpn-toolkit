//! Self-update via rename-aside.
//!
//! The pkg's postinstall `cp`s a new `/usr/local/bin/gnosis_vpn-update` over
//! the old one — but this very process is running from that file, and macOS
//! refuses to overwrite a running Mach-O image (`ETXTBSY`-like failure the
//! postinstall silently swallows), so historically the updater never actually
//! updated itself. The fix needs no packaging change: just before spawning
//! `installer(8)`, the running binary is renamed aside to
//! `gnosis_vpn-update.old`. The process keeps executing from its (renamed)
//! inode, and the postinstall's `cp` now creates a *fresh* file at the real
//! path and succeeds.
//!
//! On install success the `.old` file is deleted and the new on-disk binary's
//! `version` output is captured as proof for the audit log. On failure the
//! `.old` binary is renamed back — unless the postinstall already wrote a new
//! binary at the path, which must not be clobbered.
//!
//! Best-effort throughout: every problem only logs a warning and never fails
//! the update itself.

use std::path::{Path, PathBuf};

/// Path the pkg installs the updater binary to. Used instead of
/// `current_exe()` on purpose: the postinstall replaces this exact path, and a
/// dev binary running from somewhere else must not shuffle files around in
/// `/usr/local/bin` based on where *it* happens to live.
pub const INSTALLED_BINARY_PATH: &str = "/usr/local/bin/gnosis_vpn-update";

const OLD_SUFFIX: &str = ".old";

/// Outcome of the pre-install rename, consumed by exactly one of
/// `finish_success` / `finish_failure` after `installer(8)` returns.
#[derive(Debug)]
pub struct RenameAside {
    path: PathBuf,
    old_path: PathBuf,
    renamed: bool,
}

/// Rename the installed updater binary aside so the postinstall's `cp` can
/// create a fresh file at the real path. Call immediately before spawning
/// `installer(8)`.
pub fn rename_aside() -> RenameAside {
    if let Ok(exe) = std::env::current_exe()
        && exe != Path::new(INSTALLED_BINARY_PATH)
    {
        tracing::warn!(
            current_exe = %exe.display(),
            install_path = INSTALLED_BINARY_PATH,
            "running binary is not the installed updater; renaming the installed path aside anyway"
        );
    }
    RenameAside::engage(Path::new(INSTALLED_BINARY_PATH))
}

impl RenameAside {
    /// Path-parameterized rename-aside (unit-testable). `path` → `path.old`.
    fn engage(path: &Path) -> Self {
        let mut old_os = path.as_os_str().to_os_string();
        old_os.push(OLD_SUFFIX);
        let old_path = PathBuf::from(old_os);

        let renamed = match std::fs::rename(path, &old_path) {
            Ok(()) => {
                tracing::info!(
                    from = %path.display(),
                    to = %old_path.display(),
                    "renamed running updater binary aside so postinstall can replace it"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "cannot rename updater binary aside; postinstall will fail to replace it (as before)"
                );
                false
            }
        };
        RenameAside {
            path: path.to_path_buf(),
            old_path,
            renamed,
        }
    }

    /// Install succeeded: the postinstall has written a new binary at the real
    /// path, so the `.old` copy is dead weight — delete it.
    fn finish_success(self) {
        if !self.renamed {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.old_path) {
            tracing::warn!(error = %e, path = %self.old_path.display(), "cannot remove old updater binary");
        }
    }

    /// Install failed: put the old binary back so the system keeps a working
    /// updater — unless the postinstall got far enough to write a new binary
    /// at the path, which must not be clobbered (it is at least as good as the
    /// `.old` one).
    fn finish_failure(self) {
        if !self.renamed {
            return;
        }
        if self.path.exists() {
            tracing::warn!(
                path = %self.path.display(),
                "install failed but postinstall already replaced the updater binary; keeping it and dropping the old copy"
            );
            let _ = std::fs::remove_file(&self.old_path);
            return;
        }
        if let Err(e) = std::fs::rename(&self.old_path, &self.path) {
            tracing::warn!(
                error = %e,
                from = %self.old_path.display(),
                to = %self.path.display(),
                "install failed and the old updater binary could not be restored"
            );
        } else {
            tracing::info!(path = %self.path.display(), "install failed; restored the previous updater binary");
        }
    }
}

/// Settle the rename-aside according to the install outcome. On success,
/// additionally execute the NEW on-disk binary's `version` subcommand and
/// return its trimmed output as proof the self-update took effect (`None` and
/// a warning when the new binary is missing/not executable). The probe execs
/// the fresh file at the install path — never this process's own replaced
/// image.
pub async fn finish(aside: RenameAside, install_succeeded: bool) -> Option<String> {
    if !install_succeeded {
        aside.finish_failure();
        return None;
    }
    aside.finish_success();

    match tokio::process::Command::new(INSTALLED_BINARY_PATH)
        .arg("version")
        .arg("--output")
        .arg("plain")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            tracing::info!(version = %version, "new updater binary responds; self-update took effect");
            Some(version)
        }
        Ok(output) => {
            tracing::warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "new updater binary at {INSTALLED_BINARY_PATH} exited abnormally on version probe"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "new updater binary at {INSTALLED_BINARY_PATH} is missing or not executable"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gnosis_vpn-update-selfupd-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn engage_renames_binary_aside_and_success_removes_old() {
        let dir = TempDir::new("success");
        let bin = dir.path("gnosis_vpn-update");
        std::fs::write(&bin, "old-binary").unwrap();

        let aside = RenameAside::engage(&bin);
        assert!(aside.renamed);
        assert!(!bin.exists());
        let old = dir.path("gnosis_vpn-update.old");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "old-binary");

        // Simulate the postinstall writing the fresh binary, then settle.
        std::fs::write(&bin, "new-binary").unwrap();
        aside.finish_success();
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "new-binary");
    }

    #[test]
    fn failure_restores_old_binary_when_path_is_empty() {
        let dir = TempDir::new("restore");
        let bin = dir.path("gnosis_vpn-update");
        std::fs::write(&bin, "old-binary").unwrap();

        let aside = RenameAside::engage(&bin);
        assert!(!bin.exists());

        aside.finish_failure();
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "old-binary");
        assert!(!dir.path("gnosis_vpn-update.old").exists());
    }

    #[test]
    fn failure_does_not_clobber_a_new_binary_written_by_postinstall() {
        let dir = TempDir::new("noclobber");
        let bin = dir.path("gnosis_vpn-update");
        std::fs::write(&bin, "old-binary").unwrap();

        let aside = RenameAside::engage(&bin);
        // Postinstall replaced the binary before the install failed later.
        std::fs::write(&bin, "new-binary").unwrap();

        aside.finish_failure();
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "new-binary");
        assert!(!dir.path("gnosis_vpn-update.old").exists());
    }

    #[test]
    fn missing_binary_makes_engage_a_noop_for_both_outcomes() {
        let dir = TempDir::new("missing");
        let bin = dir.path("gnosis_vpn-update");

        let aside = RenameAside::engage(&bin);
        assert!(!aside.renamed);
        aside.finish_failure();
        assert!(!bin.exists());
        assert!(!dir.path("gnosis_vpn-update.old").exists());

        let aside = RenameAside::engage(&bin);
        aside.finish_success();
        assert!(!bin.exists());
    }
}
