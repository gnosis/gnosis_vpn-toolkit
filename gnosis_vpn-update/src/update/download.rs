//! Resumable HTTP download for the update artifact.
//!
//! The artifact is fetched over the VPN link itself — a high-latency (multi-
//! second RTT), lossy path where connection drops are routine. Instead of
//! failing on the first error, [`fetch_with_resume`] reconnects on a fixed
//! interval and continues from the current byte offset via `Range` requests,
//! giving up only after a continuous-offline budget is exhausted. Integrity is
//! not this module's job: the caller's size and SHA-256 checks remain the
//! authority over whatever bytes end up in the file.

use std::io::SeekFrom;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ETAG, HeaderMap, HeaderValue, IF_RANGE, LAST_MODIFIED, RANGE};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::time::Instant;

use super::DownloadError;

pub(super) struct RetryPolicy {
    /// Sleep between reconnect attempts while offline.
    retry_interval: Duration,
    /// Max continuous offline time per drop; resets once data flows again.
    reconnect_budget: Duration,
}

impl RetryPolicy {
    pub(super) const PROD: Self = Self {
        retry_interval: Duration::from_secs(1),
        reconnect_budget: Duration::from_secs(120),
    };
}

/// Live outage bookkeeping: set on the first failure of a drop, cleared when
/// body bytes flow again — not on a mere reconnect, so a path that completes
/// the handshake but never delivers data cannot reset the budget forever.
struct Outage {
    since: Instant,
    attempts: u64,
    last_error: String,
}

/// Fetch `url` into `file`, resuming across connection drops. Returns the
/// total bytes written. The caller owns preflight (scheme check, file
/// creation) and postflight (flush, size and SHA-256 verification).
pub(super) async fn fetch_with_resume(
    client: &reqwest::Client,
    url: &url::Url,
    file: &mut tokio::fs::File,
    expected: u64, // manifest size_bytes; 0 = unknown
    policy: &RetryPolicy,
) -> Result<u64, DownloadError> {
    let mut bytes_done: u64 = 0;
    let mut validator: Option<HeaderValue> = None;
    let mut outage: Option<Outage> = None;

    'attempt: loop {
        // A drop can lose the clean EOF after the last byte arrived; treat a
        // complete byte count as done and let the caller's checks judge it.
        if expected != 0 && bytes_done >= expected {
            return Ok(bytes_done);
        }

        if let Some(o) = &outage {
            if o.since.elapsed() >= policy.reconnect_budget {
                return Err(give_up(o, bytes_done, expected));
            }
            tokio::time::sleep(policy.retry_interval).await;
        }

        let mut request = client.get(url.clone());
        if bytes_done > 0 {
            request = request.header(RANGE, format!("bytes={bytes_done}-"));
            // If-Range makes a mid-outage republish come back as a full 200
            // (restart) instead of splicing two different artifacts.
            if let Some(v) = &validator {
                request = request.header(IF_RANGE, v.clone());
            }
        }

        // While offline, bound the in-flight attempt by the remaining budget
        // so a black-holing link cannot overshoot it by a full connect
        // timeout.
        let sent = match &outage {
            Some(o) => {
                let remaining = policy.reconnect_budget.saturating_sub(o.since.elapsed());
                match tokio::time::timeout(remaining, request.send()).await {
                    Ok(sent) => sent,
                    Err(_) => {
                        record_failure(&mut outage, "reconnect attempt timed out".into(), bytes_done, policy);
                        continue 'attempt;
                    }
                }
            }
            None => request.send().await,
        };
        let mut response = match sent {
            Ok(response) => response,
            Err(e) if is_retryable(&e) => {
                record_failure(&mut outage, e.to_string(), bytes_done, policy);
                continue 'attempt;
            }
            Err(e) => return Err(DownloadError::Http(e)),
        };

        match response.status() {
            StatusCode::PARTIAL_CONTENT if bytes_done > 0 => {}
            StatusCode::OK if bytes_done == 0 => {}
            StatusCode::OK => {
                // Server ignored the Range request (or If-Range detected a
                // changed artifact) — start over from byte 0.
                tracing::warn!(
                    bytes_done,
                    "server ignored Range request — restarting download from byte 0"
                );
                file.set_len(0).await?;
                file.seek(SeekFrom::Start(0)).await?;
                bytes_done = 0;
                validator = None;
            }
            // A status response proves connectivity is back; retrying an
            // erroring origin every second would not fix it — fail fast.
            s if s.is_client_error() || s.is_server_error() => {
                return Err(DownloadError::Http(
                    response.error_for_status().expect_err("status checked to be an error"),
                ));
            }
            s => return Err(DownloadError::UnexpectedStatus(s)),
        }
        if validator.is_none() {
            validator = strong_validator(response.headers());
        }

        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    file.write_all(&chunk).await?;
                    bytes_done += chunk.len() as u64;
                    if let Some(o) = outage.take() {
                        tracing::info!(
                            bytes_done,
                            expected,
                            attempts = o.attempts,
                            offline_secs = o.since.elapsed().as_secs(),
                            "download connection re-established — resumed"
                        );
                    }
                }
                Ok(None) => return Ok(bytes_done),
                Err(e) if is_retryable(&e) => {
                    record_failure(&mut outage, e.to_string(), bytes_done, policy);
                    continue 'attempt;
                }
                Err(e) => return Err(DownloadError::Http(e)),
            }
        }
    }
}

/// Status errors prove the connection works; builder/redirect errors are
/// structural and a retry cannot fix them. Everything else (connect failure,
/// timeout, mid-body transfer error) is a network fault worth retrying.
fn is_retryable(e: &reqwest::Error) -> bool {
    !(e.is_status() || e.is_builder() || e.is_redirect())
}

/// If-Range requires a strong validator (RFC 9110 §13.1.5) — skip weak ETags.
fn strong_validator(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .get(ETAG)
        .filter(|v| !v.as_bytes().starts_with(b"W/"))
        .or_else(|| headers.get(LAST_MODIFIED))
        .cloned()
}

fn record_failure(outage: &mut Option<Outage>, error: String, bytes_done: u64, policy: &RetryPolicy) {
    match outage {
        None => {
            tracing::warn!(
                error = %error,
                bytes_done,
                retry_secs = policy.retry_interval.as_secs(),
                budget_secs = policy.reconnect_budget.as_secs(),
                "download connection lost — reconnecting until the budget is exhausted"
            );
            *outage = Some(Outage {
                since: Instant::now(),
                attempts: 1,
                last_error: error,
            });
        }
        Some(o) => {
            o.attempts += 1;
            tracing::debug!(
                error = %error,
                attempt = o.attempts,
                offline_secs = o.since.elapsed().as_secs(),
                "reconnect attempt failed"
            );
            o.last_error = error;
        }
    }
}

fn give_up(outage: &Outage, bytes_done: u64, expected: u64) -> DownloadError {
    let offline_secs = outage.since.elapsed().as_secs();
    tracing::error!(
        offline_secs,
        bytes_done,
        expected,
        attempts = outage.attempts,
        last_error = %outage.last_error,
        "download failed: connection not re-established within budget"
    );
    DownloadError::ConnectionLost {
        offline_secs,
        bytes_done,
        expected,
        last_error: outage.last_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// What the scripted server does with connections beyond its script.
    enum AfterScript {
        /// Accept and close without responding — the "server stays down" case.
        CloseConnections,
        /// Keep replaying the last scripted response.
        RepeatLastResponse,
    }

    /// Minimal scripted HTTP/1.1 server: one raw response per connection, then
    /// close. Each received request head is forwarded (lowercased) so tests can
    /// assert on `Range`/`If-Range` headers.
    async fn spawn_server(script: Vec<Vec<u8>>, after: AfterScript) -> (SocketAddr, UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = unbounded_channel();
        tokio::spawn(async move {
            let mut script = script.into_iter();
            let mut last: Option<Vec<u8>> = None;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let response = match script.next() {
                    Some(r) => {
                        last = Some(r.clone());
                        Some(r)
                    }
                    None => match after {
                        AfterScript::CloseConnections => None,
                        AfterScript::RepeatLastResponse => last.clone(),
                    },
                };
                let Some(response) = response else {
                    continue; // drop the socket unanswered
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&head).to_lowercase());
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, rx)
    }

    /// Build a raw response; advertising more bytes than `body` carries makes
    /// the client see a mid-body connection drop.
    fn http_response(status: &str, headers: &[(&str, String)], advertised_len: usize, body: &[u8]) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {status}\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!(
            "content-length: {advertised_len}\r\nconnection: close\r\n\r\n"
        ));
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn payload() -> Vec<u8> {
        (0..1000u32).map(|i| (i % 251) as u8).collect()
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    fn fast_policy(budget_ms: u64) -> RetryPolicy {
        RetryPolicy {
            retry_interval: Duration::from_millis(10),
            reconnect_budget: Duration::from_millis(budget_ms),
        }
    }

    fn artifact_url(addr: SocketAddr) -> url::Url {
        url::Url::parse(&format!("http://{addr}/artifact.pkg")).unwrap()
    }

    fn test_file_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("gnosis_vpn-update-download-test-{name}-{}", std::process::id()))
    }

    async fn fetch(
        addr: SocketAddr,
        path: &std::path::Path,
        expected: u64,
        policy: &RetryPolicy,
    ) -> Result<u64, DownloadError> {
        let mut file = tokio::fs::File::create(path).await.unwrap();
        let result = fetch_with_resume(&test_client(), &artifact_url(addr), &mut file, expected, policy).await;
        file.flush().await.unwrap();
        result
    }

    #[tokio::test]
    async fn resumes_after_mid_body_drop() {
        let payload = payload();
        let conn1 = http_response("200 OK", &[], payload.len(), &payload[..500]);
        let conn2 = http_response(
            "206 Partial Content",
            &[("content-range", format!("bytes 500-999/{}", payload.len()))],
            500,
            &payload[500..],
        );
        let (addr, mut rx) = spawn_server(vec![conn1, conn2], AfterScript::CloseConnections).await;
        let path = test_file_path("resume-mid-drop");

        let n = fetch(addr, &path, 1000, &fast_policy(2000)).await.unwrap();

        assert_eq!(n, 1000);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), payload);
        let req1 = rx.recv().await.unwrap();
        assert!(!req1.contains("range:"));
        let req2 = rx.recv().await.unwrap();
        assert!(req2.contains("range: bytes=500-"), "resume request was: {req2}");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn sends_if_range_when_first_response_has_etag() {
        let payload = payload();
        let conn1 = http_response(
            "200 OK",
            &[("etag", "\"v1\"".to_string())],
            payload.len(),
            &payload[..500],
        );
        let conn2 = http_response(
            "206 Partial Content",
            &[("content-range", format!("bytes 500-999/{}", payload.len()))],
            500,
            &payload[500..],
        );
        let (addr, mut rx) = spawn_server(vec![conn1, conn2], AfterScript::CloseConnections).await;
        let path = test_file_path("if-range");

        let n = fetch(addr, &path, 1000, &fast_policy(2000)).await.unwrap();

        assert_eq!(n, 1000);
        let _ = rx.recv().await.unwrap();
        let req2 = rx.recv().await.unwrap();
        assert!(req2.contains("if-range: \"v1\""), "resume request was: {req2}");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn restarts_when_server_ignores_range() {
        let payload = payload();
        let conn1 = http_response("200 OK", &[], payload.len(), &payload[..500]);
        let conn2 = http_response("200 OK", &[], payload.len(), &payload);
        let (addr, _rx) = spawn_server(vec![conn1, conn2], AfterScript::CloseConnections).await;
        let path = test_file_path("range-ignored");

        let n = fetch(addr, &path, 1000, &fast_policy(2000)).await.unwrap();

        assert_eq!(n, 1000);
        // The partial 500 bytes must have been truncated away, not prepended.
        assert_eq!(tokio::fs::read(&path).await.unwrap(), payload);
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn fails_with_connection_lost_after_budget() {
        let payload = payload();
        let conn1 = http_response("200 OK", &[], payload.len(), &payload[..500]);
        let (addr, _rx) = spawn_server(vec![conn1], AfterScript::CloseConnections).await;
        let path = test_file_path("budget-exhausted");

        let err = fetch(addr, &path, 1000, &fast_policy(300)).await.unwrap_err();

        match err {
            DownloadError::ConnectionLost {
                bytes_done, expected, ..
            } => {
                assert_eq!(bytes_done, 500);
                assert_eq!(expected, 1000);
            }
            other => panic!("expected ConnectionLost, got: {other}"),
        }
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn fails_fast_on_http_status_error() {
        let conn1 = http_response("404 Not Found", &[], 0, b"");
        let (addr, mut rx) = spawn_server(vec![conn1], AfterScript::RepeatLastResponse).await;
        let path = test_file_path("status-error");

        let err = fetch(addr, &path, 1000, &fast_policy(2000)).await.unwrap_err();

        assert!(matches!(err, DownloadError::Http(_)), "got: {err}");
        assert!(err.to_string().contains("404"), "got: {err}");
        let _ = rx.recv().await.unwrap();
        assert!(rx.try_recv().is_err(), "status error must not be retried");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn completes_when_drop_lands_on_last_byte() {
        let payload = payload();
        // Advertise one byte more than we send: the client sees a mid-body
        // drop, but every expected byte has already arrived.
        let conn1 = http_response("200 OK", &[], payload.len() + 1, &payload);
        let (addr, mut rx) = spawn_server(vec![conn1], AfterScript::CloseConnections).await;
        let path = test_file_path("drop-at-eof");

        let n = fetch(addr, &path, 1000, &fast_policy(2000)).await.unwrap();

        assert_eq!(n, 1000);
        let _ = rx.recv().await.unwrap();
        assert!(rx.try_recv().is_err(), "complete download must not re-request");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn no_progress_reconnects_do_not_reset_budget() {
        // Every connection hands out headers but never a body byte — the
        // budget must keep running across these "successful" reconnects.
        let conn = http_response("200 OK", &[], 1000, b"");
        let (addr, _rx) = spawn_server(vec![conn], AfterScript::RepeatLastResponse).await;
        let path = test_file_path("no-progress");

        let result = tokio::time::timeout(Duration::from_secs(30), fetch(addr, &path, 1000, &fast_policy(300)))
            .await
            .expect("fetch must give up instead of looping forever");

        assert!(
            matches!(result, Err(DownloadError::ConnectionLost { .. })),
            "got: {result:?}"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }
}
