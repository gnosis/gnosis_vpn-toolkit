//! Update install engine.
//!
//! Ported from `gnosis_vpn-lib::update`, decoupled from the root daemon and the
//! socket IPC. The engine ([`install_engine`]) drives the
//! download → verify → install pipeline and emits [`UpdateStatus`] on a tokio
//! mpsc channel; the binary forwards each status to stdout as newline-delimited
//! JSON (see [`crate::output`]) instead of over a socket.
//!
//! On Linux the engine delegates to apt (see the `apt` module); on macOS it downloads a
//! signed `.pkg`, SHA-256-verifies it, and runs `installer(8)`.

pub mod paths;

#[cfg(target_os = "linux")]
mod apt;

use std::cmp::Ordering;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::time::SystemTime;

#[cfg(test)]
use bytesize::ByteSize;
use reqwest::Client;
use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "linux"))]
use sha2::{Digest, Sha256};
#[cfg(not(target_os = "linux"))]
use tokio::fs::OpenOptions;
#[cfg(not(target_os = "linux"))]
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::manifest::{self, Channel, ChannelRelease};

/// Install-gate failure modes — distinct from `manifest::Error`, which covers
/// the manifest-fetch path. These are the rejection reasons that apply *after*
/// a manifest is in hand and we're deciding whether a specific `ChannelRelease`
/// should be installed on this host.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    // Only constructed on the macOS engine path (`drive_engine`); on Linux the
    // check flow reports `CheckOutcome::NoReleaseForChannel` directly instead.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    #[error("Channel {0} has no release in this manifest")]
    NoReleaseForChannel(Channel),
    #[error("Candidate {candidate} requires app {required} (have {current}); upgrade to an intermediate first")]
    AppTooOld {
        current: String,
        required: String,
        candidate: String,
    },
    #[error("Candidate {candidate} is older than installed {current}; pass --allow-downgrade to override")]
    Downgrade { current: String, candidate: String },
    #[error("Candidate {candidate} is already installed")]
    AlreadyInstalled { candidate: String },
}

/// Componentwise compare for version-like strings.
///
/// Splits on `.`, `-`, and `+`, parses each component as an integer (treating
/// non-numeric chunks as `0`), and compares left-to-right padding the shorter
/// list with `0`. Used for app versions ("0.86.0") and date-based snapshot
/// versions with build metadata ("2026.04.24+build.030921"). Build metadata
/// after `+` therefore participates in the compare, which matches the
/// publishing pipeline's intent (newer builds sort higher).
///
/// Limitation: does **not** implement semver prerelease ordering (`-rc1` <
/// release). If the publishing pipeline ever adopts that style, swap this
/// for the `semver` crate.
pub fn compare_components(a: &str, b: &str) -> Ordering {
    fn parts(s: &str) -> Vec<u64> {
        s.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let a = parts(a);
    let b = parts(b);
    let n = a.len().max(b.len());
    for i in 0..n {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Less => return Ordering::Less,
            Ordering::Greater => return Ordering::Greater,
            Ordering::Equal => {}
        }
    }
    Ordering::Equal
}

/// Validate a candidate release against the currently installed app version.
///
/// `current_app_version` is supplied by the caller (the app passes it via
/// `--current-version`, sourced from the daemon's reported package version).
/// `allow_downgrade` is the explicit user override — without it,
/// strictly-lower candidates are rejected.
///
/// The manifest's `min_os_version` field is **not** consulted. On Linux the
/// manifest carries Ubuntu-style values that don't compare meaningfully
/// against Debian/Fedora versions, and the `.deb`/`.rpm` artifacts already
/// declare their real package dependencies. On macOS the `.pkg` postinstall
/// surfaces an OS-too-old failure if applicable.
pub fn ensure_installable(
    release: &ChannelRelease,
    current_app_version: &str,
    allow_downgrade: bool,
) -> Result<Ordering, GateError> {
    if compare_components(current_app_version, &release.min_app_version) == Ordering::Less {
        return Err(GateError::AppTooOld {
            current: current_app_version.to_string(),
            required: release.min_app_version.clone(),
            candidate: release.version.clone(),
        });
    }

    let ordering = compare_components(&release.version, current_app_version);
    match ordering {
        Ordering::Equal => Err(GateError::AlreadyInstalled {
            candidate: release.version.clone(),
        }),
        Ordering::Less if !allow_downgrade => Err(GateError::Downgrade {
            current: current_app_version.to_string(),
            candidate: release.version.clone(),
        }),
        _ => Ok(ordering),
    }
}

/// Stage labels carried by `UpdateStatus::Failed`. Mirrors the high-level
/// phases the engine moves through.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UpdateStage {
    Check,
    Download,
    Verify,
    Install,
}

impl std::fmt::Display for UpdateStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateStage::Check => f.write_str("check"),
            UpdateStage::Download => f.write_str("download"),
            UpdateStage::Verify => f.write_str("verify"),
            UpdateStage::Install => f.write_str("install"),
        }
    }
}

/// Streaming status emitted by the install engine. Deliberately coarse phase
/// markers — no byte-level progress — so macOS and Linux emit an **identical**
/// sequence and the app can render one UI for both. The engine streams
/// `Checking → Downloading → Installing` then a terminal `Completed`/`Failed`.
///
/// There is intentionally no download byte counter: on Linux apt downloads
/// opaquely, so a granular progress bar could never be shown there, and the two
/// platforms are kept symmetric. Rich release info (target version, notes,
/// size) is available from the separate `check-update` command
/// ([`CheckOutcome::Available`]), not from this stream. The download-finished
/// moment is signalled by the arrival of `Installing`.
///
/// Serialized to stdout as one JSON object per line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateStatus {
    /// Looking for an applicable update (manifest fetch on macOS, apt-source
    /// channel config on Linux).
    Checking,
    /// The update download has started.
    Downloading,
    /// Download finished; the platform installer is now running.
    Installing,
    /// Terminal success — the newly installed version.
    Completed { new_version: String },
    /// Terminal failure at `stage`.
    Failed { stage: UpdateStage, error: String },
}

impl UpdateStatus {
    /// `true` if no further status will follow this one (channel will close).
    pub fn is_terminal(&self) -> bool {
        matches!(self, UpdateStatus::Completed { .. } | UpdateStatus::Failed { .. })
    }

    /// `true` for a `Failed` terminal status (used to pick a process exit code).
    pub fn is_failure(&self) -> bool {
        matches!(self, UpdateStatus::Failed { .. })
    }
}

impl std::fmt::Display for UpdateStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateStatus::Checking => f.write_str("checking for updates"),
            UpdateStatus::Downloading => f.write_str("downloading update"),
            UpdateStatus::Installing => f.write_str("installing update"),
            UpdateStatus::Completed { new_version } => write!(f, "completed: {new_version}"),
            UpdateStatus::Failed { stage, error } => write!(f, "failed at {stage}: {error}"),
        }
    }
}

/// Result of a `check-update` run. Serialized as a single JSON object to
/// stdout. Mirrors the client's `CheckUpdateResponse` so the app's parsing is
/// unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CheckOutcome {
    UpToDate { current: String },
    Available { current: String, release: Box<ChannelRelease> },
    NoReleaseForChannel(Channel),
    VpnNotConnected,
    IntegrityError(String),
    Error(String),
}

/// Fetch the manifest and decide whether an update is available for `channel`,
/// relative to `current_version`. Unless `force`, requires an active VPN
/// connection (queried over the daemon socket).
pub async fn check(
    client: &Client,
    channel: Channel,
    current_version: &str,
    socket_path: &std::path::Path,
    force: bool,
) -> CheckOutcome {
    if !force && crate::vpn_status::ensure_connected(socket_path).await.is_err() {
        return CheckOutcome::VpnNotConnected;
    }

    let manifest = match manifest::download(client).await {
        Ok(m) => m,
        Err(manifest::Error::Integrity(msg)) => return CheckOutcome::IntegrityError(msg),
        Err(e) => return CheckOutcome::Error(e.to_string()),
    };

    let Some(release) = manifest.pick(channel).cloned() else {
        return CheckOutcome::NoReleaseForChannel(channel);
    };

    match ensure_installable(&release, current_version, false) {
        Ok(_) => CheckOutcome::Available {
            current: current_version.to_string(),
            release: Box::new(release),
        },
        Err(GateError::AlreadyInstalled { .. }) | Err(GateError::Downgrade { .. }) => CheckOutcome::UpToDate {
            current: current_version.to_string(),
        },
        Err(e) => CheckOutcome::Error(e.to_string()),
    }
}

/// Engine input. Constructed by the binary before spawning the engine task.
///
/// On Linux only `channel`, `socket_path`, and `skip_vpn_check` are consumed
/// (the rest drive the macOS download/verify/install path), so the unread
/// fields are expected there.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Clone, Debug)]
pub struct EngineInput {
    /// HTTPS client to use for manifest + artifact fetch.
    pub client: Client,
    /// Channel to install from.
    pub channel: Channel,
    /// Whether to permit installing an older release.
    pub allow_downgrade: bool,
    /// Currently-installed app version (from `--current-version`).
    pub current_app_version: String,
    /// Root-owned directory where the artifact is downloaded.
    pub download_dir: PathBuf,
    /// Optional path to write `last_update_attempt.json` to.
    pub attempt_state_path: Option<PathBuf>,
    /// Optional audit log path; appended to on every terminal status.
    pub audit_log_path: Option<PathBuf>,
    /// Bypass the VPN-connected gate. False in production.
    pub skip_vpn_check: bool,
    /// Socket path for the VPN-connected check.
    pub socket_path: PathBuf,
}

/// Spawn the install engine task and return an `mpsc::Receiver` that yields
/// each `UpdateStatus` until terminal, then closes.
///
/// The engine emits the same coarse sequence on every platform:
/// 1. `Checking` → resolve what to install (macOS: manifest fetch + integrity,
///    SHA only for now, PGP TODO; Linux: apt-source channel config)
/// 2. `Downloading` → the artifact download has started (macOS: `.pkg` fetch +
///    silent SHA-256 verify; Linux: apt fetch)
/// 3. `Installing` → download finished, platform installer running (macOS:
///    `installer(8)`; Linux: `apt-get install --only-upgrade`)
/// 4. terminal `Completed { new_version }`
///
/// On any failure the engine emits `Failed { stage, error }` and stops. The
/// `stage` may be finer-grained than the status stream (e.g. `Verify` on a
/// macOS SHA mismatch, `Download` on a Linux `apt-get update` failure).
///
/// On Linux the engine delegates to apt — see the `apt` module. The `channel`,
/// `socket_path`, and `skip_vpn_check` fields are honoured there.
/// `download_dir`, `attempt_state_path`, `audit_log_path`, `allow_downgrade`,
/// and `current_app_version` are macOS-only.
pub fn install_engine(input: EngineInput) -> mpsc::Receiver<UpdateStatus> {
    #[cfg(target_os = "linux")]
    {
        apt::install_engine(input.channel, input.socket_path, input.skip_vpn_check)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move { run_engine(input, tx).await });
        rx
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_engine(input: EngineInput, tx: mpsc::Sender<UpdateStatus>) {
    let outcome = drive_engine(&input, &tx).await;
    let last = match outcome {
        Ok(version) => UpdateStatus::Completed { new_version: version },
        Err((stage, error)) => UpdateStatus::Failed { stage, error },
    };
    persist_attempt(&input, &last).await;
    audit_log(&input, &last).await;
    let _ = tx.send(last).await;
}

#[cfg(not(target_os = "linux"))]
async fn drive_engine(input: &EngineInput, tx: &mpsc::Sender<UpdateStatus>) -> Result<String, (UpdateStage, String)> {
    let _ = tx.send(UpdateStatus::Checking).await;

    if !input.skip_vpn_check {
        crate::vpn_status::ensure_connected(&input.socket_path)
            .await
            .map_err(|e| (UpdateStage::Check, e.to_string()))?;
    }
    let manifest = manifest::download(&input.client)
        .await
        .map_err(|e| (UpdateStage::Check, e.to_string()))?;

    let release = manifest.pick(input.channel).cloned().ok_or_else(|| {
        (
            UpdateStage::Check,
            GateError::NoReleaseForChannel(input.channel).to_string(),
        )
    })?;

    ensure_installable(&release, &input.current_app_version, input.allow_downgrade)
        .map_err(|e| (UpdateStage::Check, e.to_string()))?;

    let _ = tx.send(UpdateStatus::Downloading).await;

    let artifact_path = download_artifact(input, &release)
        .await
        .map_err(|e| (UpdateStage::Download, e.to_string()))?;

    // SHA-256 verify happens silently between download and install; a mismatch
    // surfaces as `Failed { stage: Verify }`. No dedicated status is emitted so
    // the stream stays symmetric with the Linux (apt) path.
    verify_sha256(&artifact_path, &release)
        .await
        .map_err(|e| (UpdateStage::Verify, e))?;

    let _ = tx.send(UpdateStatus::Installing).await;

    // `install_platform` only exists on macOS (Linux updates go through `apt`
    // and never reach this engine). On any other non-Linux target there is no
    // installer to invoke, so fail the Install stage cleanly rather than
    // failing to compile against a missing module.
    #[cfg(target_os = "macos")]
    {
        install_platform::install(&artifact_path)
            .await
            .map_err(|e| (UpdateStage::Install, e))?;
        Ok(release.version.clone())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err((
            UpdateStage::Install,
            "self-update is not supported on this platform".to_string(),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, thiserror::Error)]
enum DownloadError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not enough disk space: need {needed}, free {free}")]
    InsufficientSpace { needed: u64, free: u64 },
    #[error("download path is a symlink — refusing to write")]
    SymlinkDownloadPath,
    #[error("manifest size {expected} but downloaded {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("refusing to download artifact over insecure scheme: {0}")]
    InsecureUrl(String),
    #[error("manifest download URL has no safe artifact filename: {0}")]
    InvalidFilename(String),
}

#[cfg(not(target_os = "linux"))]
const FREE_SPACE_HEADROOM: u64 = 500 * 1024 * 1024; // plan: size + 500 MB headroom

/// Derive a safe on-disk artifact filename from a download URL.
///
/// Manifest PGP verification is currently disabled, so the download URL is not
/// a fully trusted input. We take the URL's last path segment but require it to
/// be a single, *normal* basename: a value like `..`, `.`, or one carrying a
/// path separator could make `download_dir.join(name)` escape the download
/// directory and let a privileged run remove/overwrite arbitrary files when it
/// later operates on the target. Returns `None` for any URL without such a
/// segment so callers can treat it as a hard failure instead of guessing a
/// fallback name.
///
/// (Gated to the targets that actually download — non-Linux — plus `test`, so
/// it is compiled and exercised by the Linux test suite without tripping a
/// dead-code lint on the Linux release build.)
#[cfg(any(not(target_os = "linux"), test))]
fn safe_artifact_filename(url: &url::Url) -> Option<String> {
    use std::path::{Component, Path};

    let segment = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())?;

    // `path_segments` yields percent-encoded ASCII, so the segment cannot itself
    // contain a literal `/`; the component check still guards `..`/`.` and any
    // separator a non-Unix `Path` would recognise.
    let mut components = Path::new(segment).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name.to_str().map(str::to_string),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
async fn download_artifact(input: &EngineInput, release: &ChannelRelease) -> Result<PathBuf, DownloadError> {
    // The privileged updater must never fetch the artifact over cleartext.
    // Manifest PGP verification is currently disabled, so an http:// URL would
    // leave the SHA256 check as the sole integrity control over an exposed
    // channel — reject anything that isn't HTTPS before issuing the request.
    if release.download_url.scheme() != "https" {
        return Err(DownloadError::InsecureUrl(release.download_url.scheme().to_string()));
    }

    tokio::fs::create_dir_all(&input.download_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&input.download_dir, std::fs::Permissions::from_mode(0o700)).await;
    }

    // Refuse to act on a download URL whose last path segment is not a safe
    // basename: a malformed URL is a hard failure rather than a guessed name.
    let filename = safe_artifact_filename(&release.download_url)
        .ok_or_else(|| DownloadError::InvalidFilename(release.download_url.to_string()))?;
    let target = input.download_dir.join(&filename);

    if let Ok(meta) = tokio::fs::symlink_metadata(&target).await {
        if meta.file_type().is_symlink() {
            return Err(DownloadError::SymlinkDownloadPath);
        }
        // pre-existing regular file: remove it so we always create_new
        let _ = tokio::fs::remove_file(&target).await;
    }

    let expected = release.size_bytes.as_u64();
    let need = expected + FREE_SPACE_HEADROOM;
    if let Some(free) = free_bytes(&input.download_dir) {
        if free < need {
            return Err(DownloadError::InsufficientSpace { needed: need, free });
        }
    }

    let mut response = input
        .client
        .get(release.download_url.clone())
        .send()
        .await?
        .error_for_status()?;

    let mut file = OpenOptions::new().write(true).create_new(true).open(&target).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600)).await;
    }

    let mut bytes_done: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
        bytes_done += chunk.len() as u64;
    }
    file.flush().await?;
    drop(file);

    if expected != 0 && bytes_done != expected {
        let _ = tokio::fs::remove_file(&target).await;
        return Err(DownloadError::SizeMismatch {
            expected,
            actual: bytes_done,
        });
    }

    Ok(target)
}

#[cfg(not(target_os = "linux"))]
async fn verify_sha256(path: &std::path::Path, release: &ChannelRelease) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    // Stream the artifact through the hasher with a fixed buffer instead of
    // reading the whole (potentially hundreds-of-MB) file into memory.
    let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hasher.finalize();
    if got.as_slice() != release.sha256.0.as_slice() {
        return Err(format!("sha256 mismatch: expected {}, got {:x}", release.sha256, got));
    }
    Ok(())
}

/// Best-effort free-space probe using `statvfs(3)` on Unix. Returns `None` if
/// the call fails — callers should treat that as "skip the check" rather than
/// blocking the install.
#[cfg(not(target_os = "linux"))]
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut buf) };
        if rc != 0 {
            return None;
        }
        Some(buf.f_bavail as u64 * buf.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Persisted so a crash mid-install can be recorded. Written on every terminal
/// status when `attempt_state_path` is set.
#[cfg(not(target_os = "linux"))]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastUpdateAttempt {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub channel: Channel,
    pub candidate_version: Option<String>,
    pub final_status: UpdateStatus,
}

#[cfg(not(target_os = "linux"))]
async fn persist_attempt(input: &EngineInput, last: &UpdateStatus) {
    let Some(path) = input.attempt_state_path.as_ref() else {
        return;
    };
    let attempt = LastUpdateAttempt {
        timestamp: SystemTime::now().into(),
        channel: input.channel,
        candidate_version: None,
        final_status: last.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&attempt) {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(path, bytes).await;
    }
}

#[cfg(not(target_os = "linux"))]
async fn audit_log(input: &EngineInput, last: &UpdateStatus) {
    let Some(path) = input.audit_log_path.as_ref() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let ts: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
    let line = format!("{ts}\tchannel={}\tstatus={}\n", input.channel, last);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path).await {
        let _ = f.write_all(line.as_bytes()).await;
    }
}

/// Platform-specific install invocation. macOS only — Linux goes through
/// the `apt` module and never calls this.
#[cfg(target_os = "macos")]
pub(crate) mod install_platform {
    use std::path::Path;
    use tokio::process::Command;

    /// Spawn `installer(8)` for the downloaded `.pkg` and wait for it to exit.
    ///
    /// The process should be ready to exit immediately after this returns: the
    /// postinstall reloads launchd which respawns the new binary.
    pub async fn install(path: &Path) -> Result<(), String> {
        let output = Command::new("installer")
            .arg("-pkg")
            .arg(path)
            .arg("-target")
            .arg("/")
            .output()
            .await
            .map_err(|e| format!("installer spawn failed: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "installer exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Hash;
    use url::Url;

    fn release(version: &str, min_app: &str, min_os: &str) -> ChannelRelease {
        ChannelRelease {
            version: version.to_string(),
            published_at: "2024-01-01T00:00:00Z".parse().unwrap(),
            download_url: Url::parse("https://download.gnosisvpn.io/artifact.pkg").unwrap(),
            size_bytes: ByteSize::mb(10),
            sha256: Hash([0u8; 32]),
            artifact_signature: String::new(),
            release_notes: String::new(),
            min_os_version: min_os.to_string(),
            min_app_version: min_app.to_string(),
        }
    }

    #[test]
    fn compare_components_handles_app_and_snapshot_versions() {
        assert_eq!(compare_components("0.86.1", "0.86.0"), Ordering::Greater);
        assert_eq!(compare_components("0.86.0", "0.86.0"), Ordering::Equal);
        assert_eq!(compare_components("0.85.0", "0.86.0"), Ordering::Less);
        // date-based snapshot version with build metadata (per the fixtures)
        assert_eq!(
            compare_components("2026.04.24+build.030922", "2026.04.24+build.030921"),
            Ordering::Greater,
        );
        assert_eq!(
            compare_components("2026.04.25+build.000001", "2026.04.24+build.999999"),
            Ordering::Greater,
        );
    }

    #[test]
    fn ensure_installable_rejects_downgrade_unless_allowed() {
        let r = release("0.85.0", "0.80.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", false).unwrap_err();
        assert!(matches!(err, GateError::Downgrade { .. }));
        let ord = ensure_installable(&r, "0.86.0", true).unwrap();
        assert_eq!(ord, Ordering::Less);
    }

    #[test]
    fn ensure_installable_rejects_when_app_too_old() {
        let r = release("1.0.0", "0.90.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", false).unwrap_err();
        assert!(matches!(err, GateError::AppTooOld { .. }));
    }

    #[test]
    fn ensure_installable_ignores_min_os_version() {
        // Linux: manifest says "22.04" but the host is "12" (Debian). No OS
        // gate runs; `dpkg`/`rpm` catch real incompatibility at install time.
        let r = release("0.87.0", "0.80.0", "22.04");
        let ord = ensure_installable(&r, "0.86.0", false).unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn ensure_installable_rejects_same_version() {
        let r = release("0.86.0", "0.80.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", false).unwrap_err();
        assert!(matches!(err, GateError::AlreadyInstalled { .. }));
    }

    #[test]
    fn ensure_installable_accepts_upgrade() {
        let r = release("0.87.0", "0.80.0", "0.0");
        let ord = ensure_installable(&r, "0.86.0", false).unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn update_status_terminal_variants() {
        assert!(
            UpdateStatus::Completed {
                new_version: "1.0.0".into()
            }
            .is_terminal()
        );
        assert!(
            UpdateStatus::Failed {
                stage: UpdateStage::Check,
                error: "x".into()
            }
            .is_terminal()
        );
        assert!(!UpdateStatus::Checking.is_terminal());
        assert!(!UpdateStatus::Downloading.is_terminal());
        assert!(!UpdateStatus::Installing.is_terminal());
    }

    #[test]
    fn update_status_roundtrips_through_json() {
        // Unit phase markers serialize as a bare tag string.
        let j = serde_json::to_string(&UpdateStatus::Downloading).expect("ser");
        assert_eq!(j, "\"Downloading\"");
        assert!(matches!(
            serde_json::from_str::<UpdateStatus>(&j).expect("de"),
            UpdateStatus::Downloading
        ));

        // A data-carrying terminal variant round-trips its payload.
        let c = UpdateStatus::Completed {
            new_version: "1.2.3".into(),
        };
        let j = serde_json::to_string(&c).expect("ser");
        let back: UpdateStatus = serde_json::from_str(&j).expect("de");
        match back {
            UpdateStatus::Completed { new_version } => assert_eq!(new_version, "1.2.3"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn safe_artifact_filename_accepts_normal_basenames() {
        let cases = [
            ("https://download.gnosisvpn.io/artifact.pkg", "artifact.pkg"),
            (
                "https://download.gnosisvpn.io/a/b/gnosis_vpn-1.2.3.dmg",
                "gnosis_vpn-1.2.3.dmg",
            ),
        ];
        for (url, expected) in cases {
            let got = safe_artifact_filename(&Url::parse(url).unwrap());
            assert_eq!(got.as_deref(), Some(expected), "url={url}");
        }
    }

    #[test]
    fn safe_artifact_filename_rejects_malformed_or_escaping_urls() {
        // Either no usable last segment, or a segment that would escape the
        // download dir. Dot-segments must never yield a name regardless of
        // whether the URL parser normalises them away or our component check
        // catches a literal `..`/`.`.
        let bad = [
            "https://download.gnosisvpn.io/",       // trailing slash → empty segment
            "https://download.gnosisvpn.io",        // no path at all
            "https://download.gnosisvpn.io/a/..",   // parent-dir traversal
            "https://download.gnosisvpn.io/..",     // parent-dir traversal
            "https://download.gnosisvpn.io/a/.",    // current-dir segment
            "https://download.gnosisvpn.io/%2e%2e", // percent-encoded `..`
            "data:text/plain,hi",                   // cannot-be-a-base URL, no path segments
        ];
        for url in bad {
            assert_eq!(safe_artifact_filename(&Url::parse(url).unwrap()), None, "url={url}");
        }
    }
}
