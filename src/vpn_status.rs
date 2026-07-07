//! Minimal, read-only client to the running `gnosis_vpn` daemon socket.
//!
//! The toolkit itself exposes no socket and talks to `gnosis_vpn-app` over
//! stdout. It does, however, need to know whether the VPN is currently
//! connected before performing an update (updating over clearnet is
//! insecure). Rather than take the whole `gnosis_vpn-lib` command surface as a
//! dependency, we mirror only the tiny slice of the wire protocol we use: send
//! the `Status` command and read the single `connected` field back.
//!
//! Maintenance note: this mirrors the daemon's serde encoding of
//! `Command::Status` / `Response::Status`. If that wire format changes in
//! `gnosis_vpn-client`, this mirror must be updated to match.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default daemon socket location (matches `gnosis_vpn-lib::socket::root`).
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/gnosisvpn.sock";
/// Environment variable overriding the socket path (matches the daemon/ctl).
pub const ENV_VAR: &str = "GNOSISVPN_SOCKET_PATH";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VPN not connected — updating without an active VPN connection is insecure")]
    NotConnected,
}

/// Request mirror. The daemon's `Command` is an externally-tagged enum, so the
/// unit variant `Status` serializes to the bare JSON string `"Status"`.
#[derive(Serialize)]
enum StatusRequest {
    Status,
}

/// Response mirror. We only ever send `Status`, so we only decode the
/// `{"Status": { .. }}` reply and read the single field we care about;
/// every other field is ignored.
#[derive(Deserialize)]
enum StatusReply {
    Status(StatusBody),
}

#[derive(Deserialize)]
struct StatusBody {
    // `connected` is `Option<..>` in the daemon: `Some` when a tunnel is up.
    // `#[serde(default)]` keeps us tolerant of the field being omitted rather
    // than serialized as `null`.
    #[serde(default)]
    connected: Option<serde::de::IgnoredAny>,
}

/// Round-trip the `Status` command over the daemon socket and report whether a
/// tunnel is currently connected.
async fn query_connected(socket_path: &Path) -> anyhow::Result<bool> {
    let mut stream = UnixStream::connect(socket_path).await?;

    let request = serde_json::to_string(&StatusRequest::Status)?;
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    // Half-close the write side so the daemon reads our command to EOF, exactly
    // as `gnosis_vpn-lib::socket::root::process_cmd` does.
    stream.shutdown().await?;

    let mut response = String::new();
    stream.read_to_string(&mut response).await?;

    let StatusReply::Status(body) = serde_json::from_str::<StatusReply>(&response)?;
    Ok(body.connected.is_some())
}

/// Succeeds only when the daemon reports an active VPN connection. Any failure
/// to reach the daemon or a not-connected status maps to
/// [`Error::NotConnected`] — the same conservative behaviour the client used.
pub async fn ensure_connected(socket_path: &Path) -> Result<(), Error> {
    match query_connected(socket_path).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::NotConnected),
        Err(e) => {
            tracing::debug!(error = %e, "failed to query daemon VPN status");
            Err(Error::NotConnected)
        }
    }
}
