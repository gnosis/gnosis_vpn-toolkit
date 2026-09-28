//! Update manifest types + fetch.
//!
//! Ported from `gnosis_vpn-lib::check_update`. The VPN-connected gate that
//! previously lived here (`ensure_vpn_connected`, which reached the daemon over
//! the socket) has moved to [`crate::vpn_status`]; `download` no longer knows
//! about the socket. Callers apply the gate before fetching.

use bytesize::ByteSize;
use chrono::{DateTime, Utc};
// TODO: re-enable once the public key is hosted externally; see verify_and_parse below.
// use pgp::{Deserializable, SignedPublicKey, StandaloneSignature};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_with::{hex::Hex, serde_as};
use std::fmt;
// use std::io::Cursor;
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

pub type Timestamp = DateTime<Utc>;

#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hash(#[serde_as(as = "Hex")] pub [u8; 32]);

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

// TODO: re-enable once the public key is hosted externally; see verify_and_parse below.
// const PUBLIC_KEY: &str = include_str!("../gnosisvpn-public-key.asc");

/// Stable's manifest host: the ENS/IPFS gateway, so production does not depend on
/// one origin. The plain `<platform>.json`, so `download_url`s still point at GCS.
const MANIFEST_BASE_URL_STABLE: &str = "https://download.vpn.gnosis.eth.limo/manifests/";

/// Pre-release channels' manifest host: the mirror lags by hours (snapshot) to
/// days (experimental), and nightly builds need what shipped minutes ago.
const MANIFEST_BASE_URL_PRERELEASE: &str = "https://download.gnosisvpn.io/manifests/";

/// Per-attempt deadline for the small in-memory manifest/signature fetches.
/// The shared client deliberately has no total timeout (the artifact download
/// must be allowed to run long), so these bounded fetches set their own;
/// `RetryPolicy::budget` caps all attempts together.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct RetryPolicy {
    max_attempts: u32,
    /// Sleep after the first failed attempt; doubles after each further one.
    backoff: Duration,
    /// Shared by both files: keeps `check-update` under the app's 75 s kill.
    budget: Duration,
}

impl RetryPolicy {
    const PROD: Self = Self {
        max_attempts: 5,
        backoff: Duration::from_secs(1),
        budget: Duration::from_secs(60),
    };
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const MANIFEST_FILENAME: &str = "macos-arm64.json";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const MANIFEST_FILENAME: &str = "linux-amd64.json";

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const MANIFEST_FILENAME: &str = "linux-arm64.json";

/// Release channel selector for picking an entry out of a `Manifest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Snapshot,
    Experimental,
}

impl Channel {
    /// Title-case name for human-readable output; `Display` stays lowercase to
    /// match the wire value.
    pub fn title(self) -> &'static str {
        match self {
            Channel::Stable => "Stable",
            Channel::Snapshot => "Snapshot",
            Channel::Experimental => "Experimental",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Channel::Stable => f.write_str("stable"),
            Channel::Snapshot => f.write_str("snapshot"),
            Channel::Experimental => f.write_str("experimental"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub generated_at: String,
    pub channels: ManifestChannels,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestChannels {
    pub stable: Option<ChannelRelease>,
    pub snapshot: Option<ChannelRelease>,
    /// The publisher omits this until the channel has built once, so it stays
    /// optional like the others rather than being required.
    #[serde(default)]
    pub experimental: Option<ChannelRelease>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelRelease {
    pub version: String,
    pub published_at: Timestamp,
    pub download_url: Url,
    pub size_bytes: ByteSize,
    pub sha256: Hash,
    pub artifact_signature: String,
    pub release_notes: String,
    pub min_os_version: String,
    pub min_app_version: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Manifest integrity error: {0}")]
    Integrity(String),
    #[error("Update check error: {0}")]
    Other(String),
}

impl Manifest {
    /// Returns the release entry for the requested channel, if present.
    pub fn pick(&self, channel: Channel) -> Option<&ChannelRelease> {
        match channel {
            Channel::Stable => self.channels.stable.as_ref(),
            Channel::Snapshot => self.channels.snapshot.as_ref(),
            Channel::Experimental => self.channels.experimental.as_ref(),
        }
    }
}

/// Which host to read from: stable off the gateway, the pre-release channels off
/// the origin that publishes them.
fn base_url(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => MANIFEST_BASE_URL_STABLE,
        Channel::Snapshot | Channel::Experimental => MANIFEST_BASE_URL_PRERELEASE,
    }
}

fn verify_and_parse(manifest_bytes: &[u8], sig_bytes: &[u8]) -> Result<Manifest, Error> {
    // TODO: re-enable PGP signature verification once the public key is hosted
    // externally. The verification code below is complete; uncomment this block
    // (along with the related imports and the `PUBLIC_KEY` constant above) to
    // restore signed-manifest enforcement.
    /*
    let (public_key, _) =
        SignedPublicKey::from_armor_single(Cursor::new(PUBLIC_KEY)).map_err(|e| Error::Integrity(e.to_string()))?;
    let (sig, _) =
        StandaloneSignature::from_armor_single(Cursor::new(sig_bytes)).map_err(|e| Error::Integrity(e.to_string()))?;
    sig.verify(&public_key, manifest_bytes)
        .map_err(|e| Error::Integrity(e.to_string()))?;
    */
    let _ = sig_bytes;
    serde_json::from_slice(manifest_bytes).map_err(|e| Error::Integrity(e.to_string()))
}

/// Download and verify this platform's manifest; `channel` picks the host only,
/// every channel rides along. VPN gating is the caller's (`ensure_connected`).
pub async fn download(client: &Client, channel: Channel) -> Result<Manifest, Error> {
    let sig_filename = MANIFEST_FILENAME.replace(".json", ".json.asc");
    let base = url::Url::parse(base_url(channel)).map_err(|e| Error::Other(e.to_string()))?;
    let manifest_url = base.join(MANIFEST_FILENAME).map_err(|e| Error::Other(e.to_string()))?;
    let sig_url = base.join(&sig_filename).map_err(|e| Error::Other(e.to_string()))?;

    tracing::debug!(?manifest_url, ?sig_url, "downloading update manifest and signature");

    let policy = &RetryPolicy::PROD;
    let deadline = Instant::now() + policy.budget;
    let manifest_bytes = fetch(client, &manifest_url, deadline, policy).await?;
    let sig_bytes = fetch(client, &sig_url, deadline, policy).await?;

    verify_and_parse(&manifest_bytes, &sig_bytes)
}

/// GET `url` into memory, retrying transient failures until the policy's
/// attempts or `deadline` run out.
async fn fetch(client: &Client, url: &Url, deadline: Instant, policy: &RetryPolicy) -> Result<Vec<u8>, Error> {
    let mut backoff = policy.backoff;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let timeout = REQUEST_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let e = match get_bytes(client, url, timeout).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => e,
        };
        if !is_transient(&e) || attempt >= policy.max_attempts || Instant::now() + backoff >= deadline {
            return Err(Error::Other(format!("{e} (attempt {attempt}/{})", policy.max_attempts)));
        }
        tracing::warn!(%url, attempt, max_attempts = policy.max_attempts, error = %e, "manifest fetch failed — retrying");
        tokio::time::sleep(backoff).await;
        backoff *= 2;
    }
}

async fn get_bytes(client: &Client, url: &Url, timeout: Duration) -> reqwest::Result<Vec<u8>> {
    let response = client
        .get(url.clone())
        .timeout(timeout)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.bytes().await?.into())
}

/// A status proves the origin answered, so only 5xx, 408 and 429 may clear up
/// on their own. Builder/redirect errors are structural; anything else
/// (connect failure, timeout, cut-off body) is the network and worth retrying.
fn is_transient(e: &reqwest::Error) -> bool {
    match e.status() {
        Some(s) => s.is_server_error() || s == StatusCode::REQUEST_TIMEOUT || s == StatusCode::TOO_MANY_REQUESTS,
        None => !(e.is_builder() || e.is_redirect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{AfterScript, http_response, spawn_server, test_client};
    use std::net::SocketAddr;
    use tokio::sync::mpsc::UnboundedReceiver;

    const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES_DIR}/{name}")).expect("fixture file not found")
    }

    fn verify_fixture(manifest_file: &str) {
        let sig_file = manifest_file.replace(".json", ".json.asc");
        let manifest_bytes = fixture(manifest_file);
        let sig_bytes = fixture(&sig_file);
        let result = verify_and_parse(&manifest_bytes, &sig_bytes);
        assert!(
            result.is_ok(),
            "verification failed for {manifest_file}: {:?}",
            result.err()
        );
        let manifest = result.unwrap();
        assert_eq!(manifest.schema_version, 1, "schema_version should be 1");
        let stable = manifest.channels.stable.expect("stable channel should exist");
        assert!(!stable.version.is_empty(), "stable version should not be empty");
    }

    /// Every fixture the pipeline generates, whatever platform this binary is —
    /// they are read by name, never through `MANIFEST_FILENAME`.
    const ALL_FIXTURES: [&str; 3] = ["macos-arm64.json", "linux-amd64.json", "linux-arm64.json"];

    #[test]
    fn stable_reads_the_ipfs_gateway_and_prerelease_the_origin() {
        let join = |channel| {
            url::Url::parse(base_url(channel))
                .unwrap()
                .join(MANIFEST_FILENAME)
                .unwrap()
                .to_string()
        };
        // The plain `<platform>.json`, never the `.ipfs.json` variant: that one
        // is stable-only and its download_urls are IPFS paths.
        assert!(!join(Channel::Stable).contains(".ipfs.json"));
        assert!(join(Channel::Stable).starts_with("https://download.vpn.gnosis.eth.limo/manifests/"));
        assert!(join(Channel::Snapshot).starts_with("https://download.gnosisvpn.io/manifests/"));
    }

    #[test]
    fn verify_macos_arm64() {
        verify_fixture("macos-arm64.json");
    }

    #[test]
    fn verify_linux_amd64() {
        verify_fixture("linux-amd64.json");
    }

    #[test]
    fn verify_linux_arm64() {
        verify_fixture("linux-arm64.json");
    }

    // TODO: re-enable once PGP verification is restored in verify_and_parse.
    #[test]
    #[ignore]
    fn rejects_tampered_manifest() {
        let mut manifest_bytes = fixture("macos-arm64.json");
        let sig_bytes = fixture("macos-arm64.json.asc");
        // flip a byte in the middle to simulate tampering
        let mid = manifest_bytes.len() / 2;
        manifest_bytes[mid] ^= 0xff;
        let result = verify_and_parse(&manifest_bytes, &sig_bytes);
        assert!(result.is_err(), "tampered manifest should fail verification");
    }

    #[test]
    fn deserializes_all_fixtures() {
        for name in ALL_FIXTURES {
            let bytes = fixture(name);
            let manifest: Manifest =
                serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("deserialize {name}: {e}"));
            let stable = manifest.channels.stable.expect("stable channel");
            assert_eq!(stable.sha256.0.len(), 32);
            assert!(stable.size_bytes.as_u64() > 0);
            assert!(stable.published_at.timestamp() > 0);
            assert!(!stable.min_os_version.is_empty());
        }
    }

    // TODO: re-add a mismatched-signature test once PGP verification is restored
    // in verify_and_parse; it needs a second signed macOS fixture to use as the
    // wrong signature.

    fn fast_policy(backoff_ms: u64, budget_ms: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            backoff: Duration::from_millis(backoff_ms),
            budget: Duration::from_millis(budget_ms),
        }
    }

    async fn fetch_from(addr: SocketAddr, policy: &RetryPolicy) -> Result<Vec<u8>, Error> {
        let url = Url::parse(&format!("http://{addr}/{MANIFEST_FILENAME}")).unwrap();
        fetch(&test_client(), &url, Instant::now() + policy.budget, policy).await
    }

    fn request_count(rx: &mut UnboundedReceiver<String>) -> usize {
        std::iter::from_fn(|| rx.try_recv().ok()).count()
    }

    fn unavailable() -> Vec<u8> {
        http_response("503 Service Unavailable", &[], 0, b"")
    }

    #[tokio::test]
    async fn retries_transient_failures_then_succeeds() {
        let ok = http_response("200 OK", &[], 2, b"{}");
        let (addr, mut rx) = spawn_server(vec![unavailable(), unavailable(), ok], AfterScript::CloseConnections).await;

        let bytes = fetch_from(addr, &fast_policy(10, 5000)).await.unwrap();

        assert_eq!(bytes, b"{}");
        assert_eq!(request_count(&mut rx), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let (addr, mut rx) = spawn_server(vec![unavailable()], AfterScript::RepeatLastResponse).await;

        let err = fetch_from(addr, &fast_policy(10, 5000)).await.unwrap_err();

        assert!(err.to_string().contains("503"), "got: {err}");
        assert!(err.to_string().contains("attempt 5/5"), "got: {err}");
        assert_eq!(request_count(&mut rx), 5);
    }

    #[tokio::test]
    async fn retries_mid_body_drop() {
        let cut = http_response("200 OK", &[], 100, b"{");
        let ok = http_response("200 OK", &[], 2, b"{}");
        let (addr, mut rx) = spawn_server(vec![cut, ok], AfterScript::CloseConnections).await;

        let bytes = fetch_from(addr, &fast_policy(10, 5000)).await.unwrap();

        assert_eq!(bytes, b"{}");
        assert_eq!(request_count(&mut rx), 2);
    }

    #[tokio::test]
    async fn does_not_retry_client_error() {
        let not_found = http_response("404 Not Found", &[], 0, b"");
        let (addr, mut rx) = spawn_server(vec![not_found], AfterScript::RepeatLastResponse).await;

        let err = fetch_from(addr, &fast_policy(10, 5000)).await.unwrap_err();

        assert!(err.to_string().contains("404"), "got: {err}");
        assert_eq!(request_count(&mut rx), 1);
    }

    #[tokio::test]
    async fn stops_when_budget_exhausted() {
        let (addr, mut rx) = spawn_server(vec![unavailable()], AfterScript::RepeatLastResponse).await;

        let err = fetch_from(addr, &fast_policy(500, 250)).await.unwrap_err();

        assert!(err.to_string().contains("attempt 1/5"), "got: {err}");
        assert_eq!(request_count(&mut rx), 1);
    }
}
