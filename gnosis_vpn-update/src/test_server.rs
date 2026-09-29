use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

pub(crate) enum AfterScript {
    CloseConnections,
    RepeatLastResponse,
}

pub(crate) async fn spawn_server(script: Vec<Vec<u8>>, after: AfterScript) -> (SocketAddr, UnboundedReceiver<String>) {
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

// A larger advertised length simulates a mid-body connection drop.
pub(crate) fn http_response(status: &str, headers: &[(&str, String)], advertised_len: usize, body: &[u8]) -> Vec<u8> {
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

pub(crate) fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}
