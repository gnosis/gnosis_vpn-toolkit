//! The migration log bundle and its upload, the same protocol gnosis_vpn-app uses.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use url::Url;
use zstd::stream::Encoder;

use crate::nofollow;

pub const DEFAULT_API_URL: &str = "https://log-uploader.gnosisvpn.com/api/upload";
pub const API_URL_ENV: &str = "GNOSISVPN_LOG_UPLOAD_API_URL";

const TIMEOUT: Duration = Duration::from_secs(120);
// Mirrors the uploader's UPLOAD_HEAD_BYTES: it vets this much, then signs a URL for the whole bundle.
const HEAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 2000;
const ZSTD_LEVEL: i32 = 5;

// The uploader UTF-8-checks only the first 100 bytes; an ASCII preamble keeps user text past that boundary.
const PREAMBLE: &str = "===== Gnosis VPN migration log bundle =====\n\
    Sections: system info, user description (if given), migrate log, installer logs (if present), updates log (if present).\n\n";

pub struct BundleSources {
    pub toolkit_version: String,
    pub package_version: Option<String>,
    pub os: String,
    pub arch: String,
    pub migrate_log: PathBuf,
    pub extra_logs: Vec<PathBuf>,
}

/// Must match the uploader's `^gnosis_vpn-\d{8}-\d{6}\.log\.zst$`.
pub fn bundle_file_name(now: DateTime<Utc>) -> String {
    format!("gnosis_vpn-{}.log.zst", now.format("%Y%m%d-%H%M%S"))
}

/// One zstd frame of plain text: the uploader rejects archives.
pub fn write_bundle(description: Option<&str>, sources: &BundleSources) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder::new(Vec::new(), ZSTD_LEVEL).map_err(|e| format!("cannot create zstd encoder: {e}"))?;
    let mut text = String::from(PREAMBLE);
    let _ = write!(
        text,
        "===== system info =====\n\
         gnosis_vpn-migrate version: {}\n\
         Gnosis VPN package version: {}\n\
         OS: {} ({})\n\n",
        sources.toolkit_version,
        sources.package_version.as_deref().unwrap_or("unknown"),
        sources.os,
        sources.arch,
    );
    let description = description.map(str::trim).filter(|d| !d.is_empty());
    if let Some(description) = description {
        let capped: String = description.chars().take(MAX_DESCRIPTION_CHARS).collect();
        let _ = write!(text, "===== user description =====\n{capped}\n\n");
    }
    encoder
        .write_all(text.as_bytes())
        .map_err(|e| format!("cannot write bundle header: {e}"))?;
    write_section(&mut encoder, "migrate log", &sources.migrate_log)?;
    for path in &sources.extra_logs {
        write_section(&mut encoder, "log", path)?;
    }
    encoder.finish().map_err(|e| format!("cannot finish compression: {e}"))
}

// Headers name the file, not its path, as bundles go to support; never follow a planted symlink or block on a FIFO.
fn write_section<W: Write>(encoder: &mut Encoder<'_, W>, title: &str, path: &Path) -> Result<(), String> {
    let source = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    let file = match nofollow::open_regular_path(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "log source unreadable");
            return writeln!(encoder, "===== {title} ({source}) unavailable: {e} =====\n")
                .map_err(|e| format!("cannot write section header: {e}"));
        }
    };
    writeln!(encoder, "===== {title} ({source}) =====").map_err(|e| format!("cannot write section header: {e}"))?;
    io::copy(&mut BufReader::new(file), encoder).map_err(|e| format!("cannot compress {}: {e}", path.display()))?;
    writeln!(encoder).map_err(|e| format!("cannot write section footer: {e}"))
}

pub fn build_client(user_agent: &str) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("cannot create HTTP client: {e}"))
}

/// Two steps: POST the head and size, then PUT the whole bundle to the signed URL; returns the uuid to quote.
pub async fn send(
    client: &reqwest::Client,
    api_url: &str,
    file_name: String,
    bundle: Vec<u8>,
) -> Result<String, String> {
    let head = bundle[..bundle.len().min(HEAD_BYTES)].to_vec();
    let part = reqwest::multipart::Part::bytes(head)
        .file_name(file_name)
        .mime_str("application/zstd")
        .map_err(|e| format!("invalid upload content type: {e}"))?;
    let form = reqwest::multipart::Form::new()
        .part("head", part)
        .text("size", bundle.len().to_string());
    let response = client
        .post(api_url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| send_error("the log uploader", e))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read upload response: {e}"))?;
    let signed = parse_upload_response(status, &body)?;
    reject_transport_downgrade(api_url, &signed.url)?;

    let mut put = client.put(&signed.url).body(bundle);
    for (name, value) in &signed.headers {
        put = put.header(name, value);
    }
    let response = put.send().await.map_err(|e| send_error("Cloud Storage", e))?;
    let status = response.status();
    if status.is_success() {
        return Ok(signed.uuid);
    }
    let body: String = response.text().await.unwrap_or_default().chars().take(500).collect();
    Err(format!(
        "Cloud Storage rejected the upload (HTTP {}): {body}",
        status.as_u16()
    ))
}

// The bundle holds installer logs; a signed URL must not fall back to plain http (allowed only for local test servers).
fn reject_transport_downgrade(api_url: &str, put_url: &str) -> Result<(), String> {
    let api_is_https = Url::parse(api_url).is_ok_and(|u| u.scheme() == "https");
    let put_is_https = Url::parse(put_url).is_ok_and(|u| u.scheme() == "https");
    if api_is_https && !put_is_https {
        return Err("the log uploader returned a non-https storage URL; refusing to upload".to_string());
    }
    Ok(())
}

fn send_error(target: &str, e: reqwest::Error) -> String {
    if e.is_timeout() {
        return "upload timed out".to_string();
    }
    // The Display form embeds the URL, which on the storage leg carries the signed credentials.
    format!("could not reach {target}: {}", e.without_url())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    uuid: Option<String>,
    upload_url: Option<String>,
    upload_headers: Option<HashMap<String, String>>,
    error: Option<String>,
}

#[derive(Debug, PartialEq)]
struct SignedUpload {
    uuid: String,
    url: String,
    // The signature covers these, so the PUT must send them unchanged.
    headers: HashMap<String, String>,
}

fn parse_upload_response(status: u16, body: &str) -> Result<SignedUpload, String> {
    let parsed = serde_json::from_str::<UploadResponse>(body);
    let success = (200..300).contains(&status);
    if !success {
        // Proxies in front of the uploader (Cloud Run's 413) answer with HTML, not JSON.
        let server_error = parsed.ok().and_then(|r| r.error);
        return Err(server_error.unwrap_or_else(|| format!("upload failed (HTTP {status})")));
    }
    let response = parsed.map_err(|e| format!("upload returned malformed JSON (HTTP {status}): {e}"))?;
    let missing = |field: &str| format!("upload returned no {field} (HTTP {status})");
    let uuid = response
        .uuid
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| missing("uuid"))?;
    let url = response
        .upload_url
        .filter(|url| !url.is_empty())
        .ok_or_else(|| missing("uploadUrl"))?;
    let headers = response.upload_headers.ok_or_else(|| missing("uploadHeaders"))?;
    Ok(SignedUpload { uuid, url, headers })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read};
    use std::net::TcpListener;
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;

    #[test]
    fn https_api_refuses_a_plain_http_storage_url() {
        assert!(reject_transport_downgrade("https://api/upload", "http://storage/obj").is_err());
        assert!(reject_transport_downgrade("https://api/upload", "https://storage/obj").is_ok());
        assert!(reject_transport_downgrade("http://127.0.0.1:1/api", "http://127.0.0.1:1/obj").is_ok());
    }

    const UUID: &str = "3f2b8c1e-9a4d-4e7b-8c21-5d6f7a8b9c0d";

    fn sources(dir: &Path) -> BundleSources {
        BundleSources {
            toolkit_version: "0.5.0".to_string(),
            package_version: Some("2026.06.06+build.000005".to_string()),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            migrate_log: dir.join("migrate.log"),
            extra_logs: vec![dir.join("updates.log")],
        }
    }

    fn decode(bundle: &[u8]) -> String {
        String::from_utf8(zstd::decode_all(bundle).unwrap()).unwrap()
    }

    #[test]
    fn bundle_is_one_frame_with_every_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("migrate.log"), "INFO migrating\n").unwrap();
        std::fs::write(dir.path().join("updates.log"), "updated\n").unwrap();
        let bundle = write_bundle(Some("  it broke  "), &sources(dir.path())).unwrap();
        let text = decode(&bundle);
        assert!(text.starts_with(PREAMBLE), "{text}");
        assert!(text.contains("gnosis_vpn-migrate version: 0.5.0\n"));
        assert!(text.contains("Gnosis VPN package version: 2026.06.06+build.000005\n"));
        assert!(text.contains("OS: linux (x86_64)\n"));
        assert!(text.contains("===== user description =====\nit broke\n\n"));
        assert!(text.contains("===== migrate log (migrate.log) =====\nINFO migrating\n\n"));
        assert!(text.contains("===== log (updates.log) =====\nupdated\n\n"));
    }

    #[test]
    fn a_missing_source_is_still_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("updates.log"), "updated\n").unwrap();
        let text = decode(&write_bundle(None, &sources(dir.path())).unwrap());
        assert!(text.contains("===== migrate log (migrate.log) unavailable: "), "{text}");
        assert!(text.contains("===== log (updates.log) =====\nupdated\n\n"), "{text}");
    }

    #[test]
    fn a_planted_symlink_or_fifo_is_reported_unavailable_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "root-only bytes").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("migrate.log")).unwrap();
        let fifo = dir.path().join("fifo.log");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR,
            0,
        )
        .unwrap();
        let mut sources = sources(dir.path());
        sources.extra_logs = vec![fifo];
        let text = decode(&write_bundle(None, &sources).unwrap());
        assert!(!text.contains("root-only bytes"), "{text}");
        assert!(text.contains("===== migrate log (migrate.log) unavailable: "), "{text}");
        assert!(text.contains("===== log (fifo.log) unavailable: "), "{text}");
    }

    #[test]
    fn description_is_optional_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let text = decode(&write_bundle(None, &sources(dir.path())).unwrap());
        assert!(!text.contains("===== user description ====="));
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        let text = decode(&write_bundle(Some(&long), &sources(dir.path())).unwrap());
        assert!(text.contains(&format!("=====\n{}\n\n", "x".repeat(MAX_DESCRIPTION_CHARS))));
        assert!(!text.contains(&"x".repeat(MAX_DESCRIPTION_CHARS + 1)));
    }

    #[test]
    fn file_name_matches_the_uploader_regex() {
        let now = DateTime::parse_from_rfc3339("2026-10-08T12:24:31Z")
            .unwrap()
            .with_timezone(&Utc);
        let name = bundle_file_name(now);
        assert_eq!(name, "gnosis_vpn-20261008-122431.log.zst");
        let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
        let rest = name
            .strip_prefix("gnosis_vpn-")
            .unwrap()
            .strip_suffix(".log.zst")
            .unwrap();
        let (date, time) = rest.split_once('-').unwrap();
        assert!(date.len() == 8 && digits(date) && time.len() == 6 && digits(time));
    }

    #[test]
    fn upload_response_needs_uuid_url_and_headers() {
        let body = format!(
            r#"{{"success":true,"uuid":"{UUID}","uploadUrl":"https://storage/obj?sig=abc","uploadHeaders":{{"Content-Type":"application/zstd"}}}}"#
        );
        let signed = parse_upload_response(201, &body).unwrap();
        assert_eq!(signed.uuid, UUID);
        assert_eq!(signed.url, "https://storage/obj?sig=abc");
        assert_eq!(signed.headers["Content-Type"], "application/zstd");
        let err = parse_upload_response(200, &format!(r#"{{"uuid":"{UUID}","uploadUrl":"u"}}"#)).unwrap_err();
        assert_eq!(err, "upload returned no uploadHeaders (HTTP 200)");
        assert_eq!(
            parse_upload_response(400, r#"{"error":"bad bundle"}"#).unwrap_err(),
            "bad bundle"
        );
        assert_eq!(
            parse_upload_response(413, "<html>too large</html>").unwrap_err(),
            "upload failed (HTTP 413)"
        );
    }

    struct Received {
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Received {
        fn header(&self, name: &str) -> &str {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("no {name} header in {:?}", self.headers))
        }
    }

    // One canned (status, body) per expected connection; reads the whole body so reqwest never sees a reset.
    fn serve(responses: impl FnOnce(&str) -> Vec<(u16, String)>) -> (String, Receiver<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let responses = responses(&base);
        let (tx, rx) = channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let method = line.split_whitespace().next().unwrap().to_string();
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let Some((name, value)) = line.trim_end().split_once(':') else {
                        break;
                    };
                    headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
                }
                let length: usize = headers
                    .iter()
                    .find(|(name, _)| name == "content-length")
                    .map(|(_, value)| value.parse().unwrap())
                    .expect("requests must declare Content-Length");
                let mut received = vec![0; length];
                reader.read_exact(&mut received).unwrap();
                tx.send(Received {
                    method,
                    headers,
                    body: received,
                })
                .unwrap();
                let mut stream = reader.into_inner();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        (base, rx)
    }

    fn signed_response(base: &str, size: usize) -> String {
        format!(
            r#"{{"success":true,"uuid":"{UUID}","uploadUrl":"{base}/bucket/obj?X-Goog-Signature=abc","uploadHeaders":{{"Content-Type":"application/zstd","x-goog-content-length-range":"{size},{size}"}}}}"#
        )
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[tokio::test]
    async fn send_posts_the_head_then_puts_the_whole_bundle() {
        let bundle: Vec<u8> = (0..HEAD_BYTES + 3).map(|i| (i % 251) as u8).collect();
        let size = bundle.len();
        let (base, rx) = serve(|base| vec![(201, signed_response(base, size)), (200, String::new())]);
        let client = build_client("test").unwrap();
        let name = "gnosis_vpn-20261008-122431.log.zst".to_string();
        let uuid = send(&client, &format!("{base}/api/upload"), name, bundle.clone())
            .await
            .unwrap();
        assert_eq!(uuid, UUID);

        let post = rx.recv().unwrap();
        assert_eq!(post.method, "POST");
        let boundary = post
            .header("content-type")
            .split_once("boundary=")
            .unwrap()
            .1
            .to_string();
        let head_start = find(&post.body, b"name=\"head\"").expect("head part");
        let head_headers = String::from_utf8_lossy(&post.body[head_start..head_start + 200]).to_string();
        assert!(
            head_headers.contains("filename=\"gnosis_vpn-20261008-122431.log.zst\""),
            "{head_headers}"
        );
        assert!(head_headers.contains("application/zstd"), "{head_headers}");
        let payload_start = head_start + find(&post.body[head_start..], b"\r\n\r\n").unwrap() + 4;
        assert_eq!(
            &post.body[payload_start..payload_start + HEAD_BYTES],
            &bundle[..HEAD_BYTES]
        );
        let size_field = format!("name=\"size\"\r\n\r\n{size}\r\n--{boundary}");
        assert!(find(&post.body, size_field.as_bytes()).is_some(), "size field missing");

        let put = rx.recv().unwrap();
        assert_eq!(put.method, "PUT");
        assert_eq!(put.header("content-type"), "application/zstd");
        assert_eq!(put.header("x-goog-content-length-range"), format!("{size},{size}"));
        assert_eq!(put.body, bundle);
    }

    #[tokio::test]
    async fn a_failing_post_and_a_rejected_put_are_readable_errors() {
        let (base, _rx) = serve(|_| vec![(500, r#"{"error":"uploader down"}"#.to_string())]);
        let client = build_client("test").unwrap();
        let err = send(&client, &format!("{base}/api/upload"), "n".to_string(), vec![1, 2, 3])
            .await
            .unwrap_err();
        assert_eq!(err, "uploader down");

        let (base, _rx) = serve(|base| vec![(201, signed_response(base, 3)), (403, "denied".to_string())]);
        let err = send(&client, &format!("{base}/api/upload"), "n".to_string(), vec![1, 2, 3])
            .await
            .unwrap_err();
        assert_eq!(err, "Cloud Storage rejected the upload (HTTP 403): denied");
    }
}
