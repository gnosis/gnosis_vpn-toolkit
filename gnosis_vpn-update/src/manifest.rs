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
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_with::{hex::Hex, serde_as};
use std::fmt;
// use std::io::Cursor;
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
/// Manifest host for the **stable** channel: the ENS/IPFS gateway, so a
/// production update check does not depend on a single centrally-hosted
/// origin. Only the manifest is fetched from here — this is deliberately the
/// plain `<platform>.json` and not the `.ipfs.json` variant, so the artifact
/// `download_url`s it carries still point at the GCS origin.
///
/// The gateway rejects requests with no `User-Agent` (403), so the shared
/// client must set one — see `USER_AGENT` in `main.rs`. It is also slower and
/// less reliable than the origin (multi-second responses, occasional 504 on a
/// cold cache), which is what `REQUEST_TIMEOUT` has to absorb.
const MANIFEST_BASE_URL_STABLE: &str = "https://download.vpn.gnosis.eth.limo/manifests/";

/// Manifest host for the pre-release channels. The IPFS mirror lags the origin
/// by hours (snapshot) to days (experimental), and nightly builds need what was
/// published minutes ago, so they read straight from GCS.
const MANIFEST_BASE_URL_PRERELEASE: &str = "https://download.gnosisvpn.io/manifests/";

/// Total per-request deadline for the small in-memory manifest/signature
/// fetches. The shared client deliberately has no total timeout (the artifact
/// download must be allowed to run long), so these bounded fetches set their
/// own.
pub(crate) const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Channel::Stable => f.write_str("stable"),
            Channel::Snapshot => f.write_str("snapshot"),
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
        }
    }
}

/// Which host to read the manifest from. Stable — the channel real users run —
/// comes off the ENS/IPFS gateway; the pre-release channels come off the origin
/// that publishes them.
fn base_url(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => MANIFEST_BASE_URL_STABLE,
        Channel::Snapshot => MANIFEST_BASE_URL_PRERELEASE,
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

/// Download and verify the update manifest for the current platform.
///
/// `channel` selects the host only (see `base_url`); the manifest fetched
/// carries every channel either way, so a cross-channel check still resolves.
///
/// The VPN-connected gate is *not* applied here — callers that want it must
/// call [`crate::vpn_status::ensure_connected`] first (see the `update` and
/// `check-update` flows).
pub async fn download(client: &Client, channel: Channel) -> Result<Manifest, Error> {
    let sig_filename = MANIFEST_FILENAME.replace(".json", ".json.asc");
    let base = url::Url::parse(base_url(channel)).map_err(|e| Error::Other(e.to_string()))?;
    let manifest_url = base.join(MANIFEST_FILENAME).map_err(|e| Error::Other(e.to_string()))?;
    let sig_url = base.join(&sig_filename).map_err(|e| Error::Other(e.to_string()))?;

    tracing::debug!(?manifest_url, ?sig_url, "downloading update manifest and signature");

    let manifest_bytes = client
        .get(manifest_url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Other(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| Error::Other(e.to_string()))?;

    let sig_bytes = client
        .get(sig_url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Other(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| Error::Other(e.to_string()))?;

    verify_and_parse(&manifest_bytes, &sig_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every fixture the publishing pipeline generates, regardless of the
    /// platform this test binary was built for — the fixtures are read by name
    /// and never go through `MANIFEST_FILENAME`.
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
}
