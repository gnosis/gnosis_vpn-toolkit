//! Loader window shown while `installer(8)` runs.
//!
//! The pkg preinstall kills the "Gnosis VPN.app" UI early and the postinstall
//! only relaunches it at the very end, so during an app-triggered update the
//! user would otherwise stare at a dead desktop for the whole install. This
//! module extracts an embedded AppleScript loader app (compiled by `build.rs`
//! from `assets/update-loader.applescript`; see the crate README) into a temp
//! dir and opens it in the console user's GUI session for the duration of the
//! install.
//!
//! Handshake: `show` writes `updating <pid>` to a world-readable status file;
//! the loader app polls that file every 0.2 s and quits on the first of:
//! `done` in the file (written by `dismiss`), the client app observed gone
//! and then running again (the postinstall relaunched it), the file missing
//! for ~2 s (already cleaned up), this process's pid dead with no
//! `installer(8)` running (orphaned by a failed install), or its own ~120 s
//! timeout. The applet must never depend on this process surviving: on an
//! app-triggered update the pkg preinstall kills the app that spawned us and
//! can take this process down with it before `dismiss` runs. `dismiss` also
//! `pkill`s the loader as a fallback and cleans up the status file + temp dir.
//!
//! Everything here is best-effort cosmetics: any failure only logs a warning
//! and must never fail the update itself. On headless systems (no console
//! user) or when not running as root, `show` skips silently.
//!
//! The bundle name and every path the loader runs from must not contain the
//! substring "Gnosis VPN" (with the space): the pkg preinstall runs
//! `pkill -f "Gnosis VPN"` and would kill the loader mid-install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

const STATUS_FILE: &str = "/Library/Logs/GnosisVPN/installer/update_status";
const APP_BUNDLE_NAME: &str = "GnosisVPNUpdateLoader.app";
/// Matched against process command lines by the `dismiss` pkill fallback; the
/// loader's executable and script paths all contain it.
const PKILL_PATTERN: &str = "GnosisVPNUpdateLoader";
/// Upper bound on how long `dismiss` waits for the loader to notice `done`
/// (it polls the status file every 0.2 s) before falling back to pkill; the
/// wait returns as soon as no loader process is left.
const DISMISS_GRACE: Duration = Duration::from_millis(1500);
/// How often `dismiss` re-checks whether the loader already quit.
const DISMISS_POLL: Duration = Duration::from_millis(200);

// Compiled from assets/update-loader.applescript by build.rs (osacompile).
static LOADER_APP_ZIP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/GnosisVPNUpdateLoader.app.zip"));

/// State handed from `show` to `dismiss`. When `show` skipped (not root,
/// headless) or failed before creating anything, `dismiss` is a no-op.
#[derive(Debug)]
pub struct Loader {
    /// Temp dir holding the extracted app bundle, if extraction got that far.
    temp_dir: Option<PathBuf>,
    /// True once the status file has been written — from that point on
    /// `dismiss` must run the full teardown.
    engaged: bool,
}

impl Loader {
    fn inactive() -> Self {
        Loader {
            temp_dir: None,
            engaged: false,
        }
    }
}

/// Show the loader window. Never fails: every problem is downgraded to a
/// `tracing::warn` and an inactive `Loader`, because the loader is cosmetic
/// and must not interfere with the install.
pub async fn show() -> Loader {
    let mut loader = Loader::inactive();

    // The loader is launched via `su -l <console user>`; without root that
    // cannot work (and the status dir under /Library/Logs may not be
    // writable either).
    if unsafe { libc::geteuid() } != 0 {
        tracing::debug!("not running as root; skipping loader window");
        return loader;
    }
    let Some(user) = console_user().await else {
        tracing::debug!("no console user (headless/SSH?); skipping loader window");
        return loader;
    };

    // The pid lets the applet detect this process dying without ever writing
    // "done" (see the module doc).
    if let Err(e) = write_status(Path::new(STATUS_FILE), &format!("updating {}", std::process::id())) {
        tracing::warn!(error = %e, "cannot write loader status file; skipping loader window");
        return loader;
    }
    loader.engaged = true;

    match extract_app().await {
        Ok(dir) => {
            let app_path = dir.join(APP_BUNDLE_NAME);
            loader.temp_dir = Some(dir);
            if let Err(e) = launch(&user, &app_path).await {
                tracing::warn!(error = %e, user = %user, "cannot open loader window in console user session");
            } else {
                tracing::info!(user = %user, "loader window opened");
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot extract loader app; skipping loader window"),
    }
    loader
}

/// Tear the loader down: signal `done` through the status file, give the
/// loader a grace period to quit on its own, pkill it as a fallback, and
/// remove the status file + temp dir. Must be called on every path out of the
/// install once `show` ran — success and failure alike. Best-effort
/// throughout; never fails the update.
pub async fn dismiss(loader: Loader) {
    if !loader.engaged {
        return;
    }

    if let Err(e) = write_status(Path::new(STATUS_FILE), "done") {
        tracing::warn!(error = %e, "cannot write done to loader status file; relying on pkill");
    }
    wait_for_loader_exit(PKILL_PATTERN, DISMISS_GRACE, DISMISS_POLL).await;

    // Exit status 1 just means "no process matched" (already quit) — ignore.
    if let Err(e) = Command::new("pkill").arg("-f").arg(PKILL_PATTERN).output().await {
        tracing::warn!(error = %e, "pkill fallback for loader failed to spawn");
    }

    cleanup(Path::new(STATUS_FILE), loader.temp_dir.as_deref());
}

/// Wait until no process matches `pattern` (the loader quit on its own — it
/// often already has, having seen the relaunched app) or `grace` elapses,
/// re-checking every `poll`. pgrep exit code 1 means "no match".
async fn wait_for_loader_exit(pattern: &str, grace: Duration, poll: Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        match Command::new("pgrep").arg("-f").arg(pattern).output().await {
            Ok(out) if out.status.code() == Some(1) => return,
            // Still running, or pgrep itself misbehaved: keep the timed wait.
            Ok(_) => {}
            // A missing/unspawnable pgrep won't recover within the grace
            // period; give up on the graceful wait and let the pkill
            // fallback in `dismiss` take over.
            Err(e) => {
                tracing::warn!(error = %e, "pgrep poll for loader failed to spawn; skipping graceful wait");
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Write `contents` to the loader status file, creating the parent directory
/// as needed and making the file world-readable — the loader app runs as the
/// console user, not root.
fn write_status(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

/// Remove the status file and the extracted-bundle temp dir. Best-effort.
fn cleanup(status_file: &Path, temp_dir: Option<&Path>) {
    let _ = std::fs::remove_file(status_file);
    if let Some(dir) = temp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The user owning the GUI console, or `None` on a headless/SSH-only system
/// (at the login window `/dev/console` is owned by root, which also counts as
/// "no console user").
async fn console_user() -> Option<String> {
    let output = Command::new("stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if user.is_empty() || user == "root" {
        return None;
    }
    Some(user)
}

/// Extract the embedded loader app zip into a fresh world-readable temp dir
/// (the console user must be able to traverse and read it) and return the dir.
async fn extract_app() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let dir = PathBuf::from(format!("/tmp/gnosisvpn-update-loader-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", dir.display()))?;

    let zip_path = dir.join("loader.zip");
    std::fs::write(&zip_path, LOADER_APP_ZIP).map_err(|e| format!("write {}: {e}", zip_path.display()))?;

    // ditto preserves the bundle's permissions (in particular the executable
    // bit on Contents/MacOS/*) — no extra Rust zip dependency needed.
    let output = Command::new("ditto")
        .arg("-x")
        .arg("-k")
        .arg(&zip_path)
        .arg(&dir)
        .output()
        .await
        .map_err(|e| format!("ditto spawn failed: {e}"))?;
    let _ = std::fs::remove_file(&zip_path);
    if !output.status.success() {
        return Err(format!(
            "ditto exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let app_path = dir.join(APP_BUNDLE_NAME);
    if !app_path.exists() {
        return Err(format!("extracted zip has no {APP_BUNDLE_NAME}"));
    }
    Ok(dir)
}

/// Open the extracted app inside the console user's GUI session. `open`
/// returns as soon as LaunchServices has taken over, so this does not block
/// on the loader's lifetime.
async fn launch(user: &str, app_path: &Path) -> Result<(), String> {
    // The generated temp path never contains quotes/spaces, and `user` is
    // passed as a plain argv element to `su`, so the single-quoting below
    // cannot be escaped.
    let output = Command::new("su")
        .arg("-l")
        .arg(user)
        .arg("-c")
        .arg(format!("/usr/bin/open '{}'", app_path.display()))
        .output()
        .await
        .map_err(|e| format!("su spawn failed: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "su/open exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gnosis_vpn-update-loader-{tag}-{}", std::process::id()));
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
    fn write_status_creates_parent_and_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("status");
        let status = dir.path("nested/dir/update_status");

        // The applet reads the pid as word 2 of the "updating <pid>" line.
        let updating = format!("updating {}", std::process::id());
        write_status(&status, &updating).unwrap();
        assert_eq!(std::fs::read_to_string(&status).unwrap(), updating);
        let mode = std::fs::metadata(&status).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);

        // Overwriting with the done sentinel replaces the contents.
        write_status(&status, "done").unwrap();
        assert_eq!(std::fs::read_to_string(&status).unwrap(), "done");
    }

    #[tokio::test]
    async fn wait_for_loader_exit_returns_immediately_when_nothing_matches() {
        let start = std::time::Instant::now();
        wait_for_loader_exit(
            &format!("gnosisvpn-loader-test-{}-no-such-process", std::process::id()),
            Duration::from_secs(5),
            Duration::from_millis(200),
        )
        .await;
        // Far below the 5 s grace: the first pgrep miss must end the wait.
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn dismiss_is_a_noop_for_an_inactive_loader() {
        let start = std::time::Instant::now();
        dismiss(Loader::inactive()).await;
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn cleanup_removes_status_file_and_temp_dir() {
        let dir = TempDir::new("cleanup");
        let status = dir.path("update_status");
        std::fs::write(&status, "done").unwrap();
        let extracted = dir.path("extracted");
        std::fs::create_dir_all(extracted.join("GnosisVPNUpdateLoader.app")).unwrap();

        cleanup(&status, Some(&extracted));
        assert!(!status.exists());
        assert!(!extracted.exists());

        // Idempotent on already-clean state, with or without a temp dir.
        cleanup(&status, Some(&extracted));
        cleanup(&status, None);
    }

    #[test]
    fn loader_zip_asset_is_embedded_and_nonempty() {
        // `include_bytes!` guarantees presence at compile time; this guards
        // against build.rs producing an empty or truncated archive.
        assert!(LOADER_APP_ZIP.len() > 100);
        // Zip local-file-header magic.
        assert_eq!(&LOADER_APP_ZIP[..4], b"PK\x03\x04");
    }
}
