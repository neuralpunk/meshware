//! Local observability endpoint (MASTERPLAN §12): `GET /healthz` and
//! `GET /metrics` (Prometheus text exposition) on every node, loopback by
//! default. Deliberately a ~hundred-line HTTP/1.1 responder instead of a
//! web framework: two GET routes, no TLS, no keep-alive — the static musl
//! binary stays lean and C-free.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::daemon::Node;
use crate::error::Result;

/// Bind the observability listener. Returns the real bound address (port 0
/// is kernel-assigned in tests).
pub async fn bind(addr: SocketAddr) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Connections served at once. A scraper uses one; anything beyond a
/// handful is somebody holding sockets open (SEC-23).
const MAX_CONNECTIONS: usize = 32;
/// How long a client may take to send its request head, and to read the
/// answer. Without them, a connection that sent nothing held a task open
/// for as long as the client liked.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Serve /healthz + /metrics until shutdown. Connections are handled one
/// request at a time (`Connection: close`) — scrapers reconnect per scrape.
pub async fn serve(listener: TcpListener, node: Node, mut shutdown: watch::Receiver<bool>) {
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let conn = tokio::select! {
            c = listener.accept() => c,
            _ = shutdown.changed() => return,
        };
        match conn {
            Ok((stream, peer)) => {
                // Taken before spawning, so a flood is refused at the accept
                // loop: the connection is simply closed.
                let Ok(slot) = std::sync::Arc::clone(&slots).try_acquire_owned() else {
                    debug!(%peer, "metrics listener at capacity");
                    continue;
                };
                let node = node.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, node).await {
                        debug!("http request failed: {e}");
                    }
                    drop(slot);
                });
            }
            Err(e) => {
                warn!("http accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle(mut stream: TcpStream, node: Node) -> std::io::Result<()> {
    // Read until the end of the request head (or a small cap — these are
    // header-only GETs from curl/Prometheus).
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read_head = async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                break;
            }
        }
        std::io::Result::Ok(())
    };
    match tokio::time::timeout(READ_TIMEOUT, read_head).await {
        Ok(read) => read?,
        Err(_) => return Ok(()),
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let (status, content_type, body) = if method != "GET" {
        (
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed\n".to_string(),
        )
    } else {
        match path {
            "/healthz" => {
                let (healthy, body) = node.healthz();
                let status = if healthy {
                    "200 OK"
                } else {
                    "503 Service Unavailable"
                };
                (status, "application/json", body)
            }
            "/metrics" => (
                "200 OK",
                "text/plain; version=0.0.4; charset=utf-8",
                node.metrics_text(),
            ),
            _ => (
                "404 Not Found",
                "text/plain",
                "not found (try /healthz or /metrics)\n".to_string(),
            ),
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    };
    match tokio::time::timeout(WRITE_TIMEOUT, write).await {
        Ok(written) => written,
        Err(_) => Ok(()),
    }
}
