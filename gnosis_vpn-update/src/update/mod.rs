//! Update install engine.
//!
//! Ported from `gnosis_vpn-lib::update`, decoupled from the root daemon and the
//! socket IPC. The engine ([`install_engine`]) drives the
//! download → verify → install pipeline and emits [`UpdateStatus`] on a tokio
//! mpsc channel; the binary forwards each status to stdout as newline-delimited
//! JSON (see [`crate::output`]) instead of over a socket.
//!
//! The engine downloads a signed `.pkg`, SHA-256-verifies it, and runs
//! `installer(8)`. macOS only.

pub mod choices;
mod download;
#[cfg(target_os = "macos")]
pub mod loader;
pub mod paths;
#[cfg(target_os = "macos")]
pub mod self_update;

use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg(test)]
use bytesize::ByteSize;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::manifest::{self, Channel, ChannelRelease};

/// Install-gate failure modes — distinct from `manifest::Error`, which covers
/// the manifest-fetch path. These are the rejection reasons that apply *after*
/// a manifest is in hand and we're deciding whether a specific `ChannelRelease`
/// should be installed on this host.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
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

/// Read the currently-installed client version from the version file the
/// installer writes (see [`paths::installed_version_path`]). Surrounding
/// whitespace/newline is trimmed; a missing or empty file is an error — the
/// updater cannot gate already-installed/downgrade without it.
pub fn read_installed_version(path: &std::path::Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read installed version from {}: {e}", path.display()))?;
    let version = raw.trim();
    if version.is_empty() {
        return Err(format!("installed version file {} is empty", path.display()));
    }
    Ok(version.to_string())
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

/// Infer which channel a version string was published on.
///
/// Stable releases are plain dotted-numeric semver taken from the repo's
/// `package.json` ("0.81.2"). Every other build line (snapshot, pr, commit)
/// carries extra metadata — `+build.…`, `+pr.…`, `+commit.…` — and parts of
/// the publishing pipeline slug the `+` to `-` for registries that reject it
/// ("0.81.2-pr.305"), so the metadata separator cannot be relied on. Treat
/// anything that is not purely digits-and-dots as a snapshot-line build.
pub fn channel_of_version(version: &str) -> Channel {
    let is_plain_release = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if is_plain_release {
        Channel::Stable
    } else {
        Channel::Snapshot
    }
}

/// Validate a candidate release against the currently installed app version.
///
/// `current_app_version` is read from the installer-written version file
/// (see [`read_installed_version`]).
/// `allow_downgrade` is the explicit user override — without it,
/// strictly-lower candidates are rejected.
///
/// A cross-channel switch (stable ⇄ snapshot, detected by comparing
/// `target_channel` against [`channel_of_version`] of the installed version)
/// is always permitted: the two channels use incomparable version schemes
/// (semver vs date+build), so the min-app / already-installed / downgrade
/// gates only apply within a channel.
///
/// The manifest's `min_os_version` field is **not** consulted here: the macOS
/// `.pkg` postinstall surfaces an OS-too-old failure at install time if
/// applicable.
pub fn ensure_installable(
    release: &ChannelRelease,
    current_app_version: &str,
    target_channel: Channel,
    allow_downgrade: bool,
) -> Result<Ordering, GateError> {
    let ordering = compare_components(&release.version, current_app_version);
    if channel_of_version(current_app_version) != target_channel {
        return Ok(ordering);
    }

    if compare_components(current_app_version, &release.min_app_version) == Ordering::Less {
        return Err(GateError::AppTooOld {
            current: current_app_version.to_string(),
            required: release.min_app_version.clone(),
            candidate: release.version.clone(),
        });
    }

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
/// markers — no byte-level progress — so the app can render a simple, stable
/// UI. The engine streams `Checking → Downloading → Installing` then a terminal
/// `Completed`/`Failed`.
///
/// There is intentionally no download byte counter. Rich release info (target
/// version, notes, size) is available from the separate `check-update` command
/// ([`CheckOutcome::Available`]), not from this stream. The download-finished
/// moment is signalled by the arrival of `Installing`.
///
/// Serialized to stdout as one JSON object per line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum UpdateStatus {
    /// Looking for an applicable update (manifest fetch + integrity check).
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
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        release: Box<ChannelRelease>,
    },
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

    match ensure_installable(&release, current_version, channel, false) {
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
#[derive(Clone, Debug)]
pub struct EngineInput {
    /// HTTPS client to use for manifest + artifact fetch.
    pub client: Client,
    /// Channel to install from.
    pub channel: Channel,
    /// Whether to permit installing an older release.
    pub allow_downgrade: bool,
    /// Currently-installed app version (from the installer's version file).
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
/// The engine emits this coarse sequence:
/// 1. `Checking` → resolve what to install (manifest fetch + integrity; SHA
///    only for now, PGP TODO)
/// 2. `Downloading` → the `.pkg` download has started, followed by a silent
///    SHA-256 verify
/// 3. `Installing` → download finished, `installer(8)` running
/// 4. terminal `Completed { new_version }`
///
/// On any failure the engine emits `Failed { stage, error }` and stops. The
/// `stage` may be finer-grained than the status stream (e.g. `Verify` on a
/// SHA mismatch).
pub fn install_engine(input: EngineInput) -> mpsc::Receiver<UpdateStatus> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move { run_engine(input, tx).await });
    rx
}

async fn run_engine(input: EngineInput, tx: mpsc::Sender<UpdateStatus>) {
    let outcome = drive_engine(&input, &tx).await;
    let (last, updater_version) = match outcome {
        Ok(outcome) => (
            UpdateStatus::Completed {
                new_version: outcome.version,
            },
            outcome.updater_version,
        ),
        Err((stage, error)) => (UpdateStatus::Failed { stage, error }, None),
    };
    persist_attempt(&input, &last).await;
    audit_log(&input, &last, updater_version.as_deref()).await;
    let _ = tx.send(last).await;
}

/// Successful engine outcome: the installed release version, plus the new
/// on-disk updater binary's own `version` output when the post-install
/// self-update probe succeeded (see `self_update`) — recorded in the audit
/// log as proof the updater replaced itself.
struct EngineOutcome {
    version: String,
    updater_version: Option<String>,
}

async fn drive_engine(
    input: &EngineInput,
    tx: &mpsc::Sender<UpdateStatus>,
) -> Result<EngineOutcome, (UpdateStage, String)> {
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

    ensure_installable(
        &release,
        &input.current_app_version,
        input.channel,
        input.allow_downgrade,
    )
    .map_err(|e| (UpdateStage::Check, e.to_string()))?;

    let _ = tx.send(UpdateStatus::Downloading).await;

    let artifact_path = download_artifact(input, &release)
        .await
        .map_err(|e| (UpdateStage::Download, e.to_string()))?;

    // SHA-256 verify happens silently between download and install; a mismatch
    // surfaces as `Failed { stage: Verify }`. No dedicated status is emitted so
    // the stream stays coarse.
    verify_sha256(&artifact_path, &release)
        .await
        .map_err(|e| (UpdateStage::Verify, e))?;

    // Pin the installer choices *before* showing the loader so a failure here
    // never leaves the window up, and the show→dismiss span below stays a
    // straight line with no early return.
    let choice_changes_path = write_choice_changes(&choices::InstallerChoices::detect(), &input.download_dir)
        .await
        .map_err(|e| (UpdateStage::Install, e))?;

    // From here on the install must run to completion: on an app-triggered
    // update the pkg preinstall kills the app that spawned this process and
    // the collateral signals must not take the updater down mid-install —
    // a half-applied install is worse than an ignored signal, and dying here
    // orphans the loader window, skips the self-update settling, and loses
    // the attempt/audit records. The process exits naturally right after the
    // engine finishes, so the dispositions are never restored.
    #[cfg(target_os = "macos")]
    shield_from_termination();

    // Show the loader window before announcing `Installing`. It must be
    // dismissed on every path out of the install — success and failure alike —
    // so no `?`/early return is allowed between here and `dismiss` below.
    #[cfg(target_os = "macos")]
    let loader = loader::show().await;

    let _ = tx.send(UpdateStatus::Installing).await;

    // Rename the running binary aside so the pkg postinstall's `cp` of the new
    // updater succeeds instead of silently failing on the running Mach-O.
    #[cfg(target_os = "macos")]
    let aside = self_update::rename_aside();

    let install_result = install_platform::install(&artifact_path, choice_changes_path.as_deref()).await;

    // Dismiss as soon as installer(8) returns: on CLI installs the postinstall
    // relaunches the app *mid-postinstall* — before installer(8) even exits —
    // and the self-update probe below can stall for seconds on the fresh
    // binary's first exec (Gatekeeper). The loader must not outlive the app's
    // reappearance any longer than it has to.
    #[cfg(target_os = "macos")]
    loader::dismiss(loader).await;

    // Settle the rename-aside on both outcomes (failure restores the old
    // binary) — which is why `install_result` is not `?`-ed until after it.
    #[cfg(target_os = "macos")]
    let updater_version = self_update::finish(aside, install_result.is_ok()).await;
    #[cfg(not(target_os = "macos"))]
    let updater_version = None;

    install_result.map_err(|e| (UpdateStage::Install, e))?;
    Ok(EngineOutcome {
        version: release.version.clone(),
        updater_version,
    })
}

/// Write the choice-changes plist pinning the installed network/loglevel
/// selections into `dir` and return its path, so the pkg's default choices
/// (jura, debug) don't flip them on a CLI install — see `choices`. `None`
/// (with a warning) when no installed selection was detected.
async fn write_choice_changes(
    installed: &choices::InstallerChoices,
    dir: &std::path::Path,
) -> Result<Option<PathBuf>, String> {
    match installed.to_choice_changes_xml() {
        Some(xml) => {
            tracing::info!(
                network = ?installed.network,
                loglevel = ?installed.loglevel,
                "pinning installer choices to the installed selection"
            );
            let path = dir.join("choice_changes.xml");
            tokio::fs::write(&path, xml)
                .await
                .map_err(|e| format!("cannot write installer choice changes: {e}"))?;
            Ok(Some(path))
        }
        None => {
            tracing::warn!("no installed network/loglevel selection detected; installer defaults apply");
            Ok(None)
        }
    }
}

/// Ignore SIGTERM/SIGINT/SIGHUP for the rest of the process's life. `SIG_IGN`
/// dispositions are process-wide and inherited-safe (no handler code runs, so
/// this is compatible with `panic = "abort"`); SIGKILL cannot be shielded —
/// the loader applet's own exit conditions cover that case.
#[cfg(target_os = "macos")]
fn shield_from_termination() {
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

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
    #[error("connection lost: gave up after {offline_secs}s offline at byte {bytes_done} of {expected}: {last_error}")]
    ConnectionLost {
        offline_secs: u64,
        bytes_done: u64,
        expected: u64,
        last_error: String,
    },
    #[error("unexpected http status: {0}")]
    UnexpectedStatus(reqwest::StatusCode),
}

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
    if let Some(free) = free_bytes(&input.download_dir)
        && free < need
    {
        return Err(DownloadError::InsufficientSpace { needed: need, free });
    }

    let mut file = OpenOptions::new().write(true).create_new(true).open(&target).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600)).await;
    }

    // The download runs over the VPN link itself — high latency, routine
    // connection drops. fetch_with_resume retries and continues via HTTP Range
    // requests instead of failing on the first drop; on error the partial file
    // is removed so a re-run starts fresh (no cross-run resume).
    let bytes_done = match download::fetch_with_resume(
        &input.client,
        &release.download_url,
        &mut file,
        expected,
        &download::RetryPolicy::PROD,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => {
            drop(file);
            let _ = tokio::fs::remove_file(&target).await;
            return Err(e);
        }
    };
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
// statvfs field widths differ per platform (on macOS `f_bavail` is u32 and
// `f_frsize` u64, elsewhere both may already be u64), so one of the two casts
// below is always "unnecessary" for the current target — keep both for
// portability and overflow-free multiplication.
#[allow(clippy::unnecessary_cast)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastUpdateAttempt {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub channel: Channel,
    pub candidate_version: Option<String>,
    pub final_status: UpdateStatus,
}

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

/// Append one line per terminal status. `updater_version` is the new on-disk
/// updater binary's `version` output (only on a successful install with a
/// successful self-update probe) — its presence in the log is the proof that
/// the updater replaced itself.
async fn audit_log(input: &EngineInput, last: &UpdateStatus, updater_version: Option<&str>) {
    let Some(path) = input.audit_log_path.as_ref() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let ts: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
    let mut line = format!("{ts}\tchannel={}\tstatus={}", input.channel, last);
    if let Some(version) = updater_version {
        line.push_str(&format!("\tupdater_version={version}"));
    }
    line.push('\n');
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path).await {
        let _ = f.write_all(line.as_bytes()).await;
    }
}

/// The macOS install invocation (runs `installer(8)`).
#[cfg(target_os = "macos")]
pub(crate) mod install_platform {
    use std::path::Path;
    use tokio::process::Command;

    /// Spawn `installer(8)` for the downloaded `.pkg` and wait for it to exit.
    ///
    /// `choice_changes` is an optional choice-changes plist passed via
    /// `-applyChoiceChangesXML` to pin the distribution package's choices
    /// (network, log level) to the installed selection instead of the
    /// package defaults (see [`super::choices`]).
    ///
    /// The process should be ready to exit immediately after this returns: the
    /// postinstall reloads launchd which respawns the new binary.
    ///
    /// INVARIANT: once this returns, the process is running from a
    /// replaced/unlinked image — `self_update` renamed the on-disk binary
    /// aside just before the spawn, and the pkg postinstall wrote a *new*
    /// binary at `/usr/local/bin/gnosis_vpn-update`. All remaining work
    /// (loader dismiss — which deliberately runs before the probe so the
    /// window closes as soon as the install ends — `persist_attempt`,
    /// `audit_log`, the terminal NDJSON emit) must not re-read or re-exec
    /// this process's own binary; the only deliberate exec afterwards is
    /// `self_update::finish` probing the NEW on-disk binary's `version`.
    pub async fn install(path: &Path, choice_changes: Option<&Path>) -> Result<(), String> {
        use std::process::Stdio;
        use std::time::Duration;

        let mut command = Command::new("installer");
        command.arg("-pkg").arg(path).arg("-target").arg("/");
        if let Some(changes) = choice_changes {
            command.arg("-applyChoiceChangesXML").arg(changes);
        }

        // Wait for the CHILD to exit, not for pipe EOF (`output()` waits for
        // the latter): anything the install scripts leave behind holding a
        // dup of these pipes would otherwise block the update — and the
        // loader's dismissal — indefinitely. The pipes only feed the error
        // message below, so draining them is best-effort with a short cap.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("installer spawn failed: {e}"))?;
        let stdout_task = drain(child.stdout.take());
        let stderr_task = drain(child.stderr.take());
        let status = child.wait().await.map_err(|e| format!("installer wait failed: {e}"))?;

        const DRAIN_CAP: Duration = Duration::from_secs(2);
        let stdout = tokio::time::timeout(DRAIN_CAP, stdout_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        let stderr = tokio::time::timeout(DRAIN_CAP, stderr_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        if status.success() {
            Ok(())
        } else {
            // installer(8) reports failures on stdout (and in
            // /var/log/install.log); stderr is usually empty.
            let detail = [stdout.trim(), stderr.trim()]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ");
            let detail = if detail.is_empty() {
                "no output; see /var/log/install.log".to_string()
            } else {
                detail
            };
            Err(format!("installer exited with {status}: {detail}"))
        }
    }

    /// Read `pipe` to a lossy string in the background so the child never
    /// stalls on a full pipe. The task is detached: if a stray fd-holder
    /// keeps the pipe open past the child's exit, `install`'s drain cap
    /// abandons it instead of waiting.
    fn drain<R>(pipe: Option<R>) -> tokio::task::JoinHandle<String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                use tokio::io::AsyncReadExt;
                let _ = pipe.read_to_end(&mut buf).await;
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
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
    fn read_installed_version_trims_and_rejects_missing_or_empty() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gnosis_vpn-update-test-version-{}.txt", std::process::id()));

        std::fs::write(&path, "0.91.1\n").unwrap();
        assert_eq!(read_installed_version(&path).unwrap(), "0.91.1");

        std::fs::write(&path, "  \n").unwrap();
        assert!(read_installed_version(&path).unwrap_err().contains("empty"));

        std::fs::remove_file(&path).unwrap();
        assert!(read_installed_version(&path).unwrap_err().contains("cannot read"));
    }

    #[tokio::test]
    async fn write_choice_changes_writes_xml_and_returns_its_path() {
        let dir = std::env::temp_dir().join(format!("gnosis_vpn-update-test-choices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let installed = choices::InstallerChoices {
            network: Some("rotsee".to_string()),
            loglevel: Some("info".to_string()),
        };
        let path = write_choice_changes(&installed, &dir).await.unwrap().unwrap();
        assert_eq!(path, dir.join("choice_changes.xml"));
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(xml.contains("rotsee"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn write_choice_changes_is_none_without_installed_selection_and_errs_on_bad_dir() {
        let no_selection = choices::InstallerChoices::default();
        assert_eq!(
            write_choice_changes(&no_selection, &std::env::temp_dir())
                .await
                .unwrap(),
            None
        );

        let installed = choices::InstallerChoices {
            network: Some("rotsee".to_string()),
            loglevel: None,
        };
        let missing_dir = std::env::temp_dir().join(format!("gnosis_vpn-update-test-nodir-{}", std::process::id()));
        let err = write_choice_changes(&installed, &missing_dir).await.unwrap_err();
        assert!(err.contains("cannot write installer choice changes"));
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
    fn channel_of_version_detects_snapshot_build_metadata() {
        assert_eq!(channel_of_version("0.91.1"), Channel::Stable);
        assert_eq!(channel_of_version("2026.06.06+build.000005"), Channel::Snapshot);
        // Registry-slugged forms replace `+` with `-`; still not stable.
        assert_eq!(channel_of_version("2026.06.06-build.000005"), Channel::Snapshot);
        assert_eq!(channel_of_version("0.81.2-pr.305"), Channel::Snapshot);
        assert_eq!(channel_of_version("0.81.2+commit.abc1234"), Channel::Snapshot);
        // Degenerate values must not be mistaken for a stable release.
        assert_eq!(channel_of_version(""), Channel::Snapshot);
        assert_eq!(channel_of_version("0..1"), Channel::Snapshot);
    }

    #[test]
    fn ensure_installable_skips_version_gates_across_channels() {
        // On snapshot, switching to stable is always allowed — even to a
        // release that would compare as a downgrade or already-installed.
        let r = release("0.85.0", "0.80.0", "0.0");
        assert!(ensure_installable(&r, "2026.06.06+build.000005", Channel::Stable, false).is_ok());

        // On stable, switching to snapshot is likewise ungated, including the
        // min-app gate (its version scheme is incomparable across channels).
        let r = release("2026.06.06+build.000005", "2026.01.01+build.000001", "0.0");
        assert!(ensure_installable(&r, "0.86.0", Channel::Snapshot, false).is_ok());
    }

    #[test]
    fn ensure_installable_gates_within_snapshot_channel() {
        let r = release("2026.06.06+build.000005", "0.0", "0.0");
        let err = ensure_installable(&r, "2026.06.06+build.000005", Channel::Snapshot, false).unwrap_err();
        assert!(matches!(err, GateError::AlreadyInstalled { .. }));
        let err = ensure_installable(&r, "2026.07.01+build.000001", Channel::Snapshot, false).unwrap_err();
        assert!(matches!(err, GateError::Downgrade { .. }));
        let ord = ensure_installable(&r, "2026.05.01+build.000009", Channel::Snapshot, false).unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn ensure_installable_rejects_downgrade_unless_allowed() {
        let r = release("0.85.0", "0.80.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", Channel::Stable, false).unwrap_err();
        assert!(matches!(err, GateError::Downgrade { .. }));
        let ord = ensure_installable(&r, "0.86.0", Channel::Stable, true).unwrap();
        assert_eq!(ord, Ordering::Less);
    }

    #[test]
    fn ensure_installable_rejects_when_app_too_old() {
        let r = release("1.0.0", "0.90.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", Channel::Stable, false).unwrap_err();
        assert!(matches!(err, GateError::AppTooOld { .. }));
    }

    #[test]
    fn ensure_installable_ignores_min_os_version() {
        // The manifest's min_os_version is not gated here; the macOS `.pkg`
        // postinstall catches real incompatibility at install time.
        let r = release("0.87.0", "0.80.0", "22.04");
        let ord = ensure_installable(&r, "0.86.0", Channel::Stable, false).unwrap();
        assert_eq!(ord, Ordering::Greater);
    }

    #[test]
    fn ensure_installable_rejects_same_version() {
        let r = release("0.86.0", "0.80.0", "0.0");
        let err = ensure_installable(&r, "0.86.0", Channel::Stable, false).unwrap_err();
        assert!(matches!(err, GateError::AlreadyInstalled { .. }));
    }

    #[test]
    fn ensure_installable_accepts_upgrade() {
        let r = release("0.87.0", "0.80.0", "0.0");
        let ord = ensure_installable(&r, "0.86.0", Channel::Stable, false).unwrap();
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
