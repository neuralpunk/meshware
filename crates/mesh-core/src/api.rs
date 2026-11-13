//! The HTTP API (E1-P6): the same signed request, over a second transport.
//!
//! ## What this is, and the one design decision that shapes it
//!
//! Everything meshware can do already goes through one door — a
//! [`SignedIpc`] envelope carrying a postcard-encoded [`IpcRequest`],
//! authenticated by the *user key that signed it*, dispatched by
//! [`Node::handle_signed`]. The CLI is a client of that door, Director is a
//! client of that door. There
//! is no private API and never was.
//!
//! So this module is **transport and nothing else**. It reads an envelope
//! off a TCP socket instead of a Unix socket and hands it to the same
//! function. That single decision is what makes an HTTP API safe to add to
//! a governance system at all:
//!
//! - **No new authorization model.** RBAC, session policies and every
//!   audit action apply because it is literally the same dispatch. A gate
//!   that exists on the socket cannot be missing here.
//! - **No new credential.** No bearer token, no API key, no session cookie.
//!   The credential is an Ed25519 key, and E1-P4's service accounts are the
//!   intended holder — a pipeline gets a short-lived machine principal, not
//!   a copy of somebody's identity file.
//! - **No new replay protection.** [`SignedIpc`] already carries a
//!   timestamp and a nonce, and `verify_envelope` already enforces a skew
//!   window and a per-`(user, nonce)` replay guard.
//!
//! The cost is that a request has to be *signed*, so `curl` alone cannot
//! make one. That is the right trade and it is deliberate: an unsigned
//! request would need a bearer token to authenticate it, and the moment
//! meshware has bearer tokens it has a credential that can be stolen from a
//! log, a proxy, or a CI environment listing. Programs are the audience for
//! an API, and a program can sign.
//!
//! ## Off by default, and on its own listener
//!
//! `[api] listen` is unset unless an operator sets it. It is deliberately
//! **not** the `/healthz` + `/metrics` listener, which is unauthenticated
//! by design: sharing a port would mean that exposing the API to a network
//! silently exposed the metrics too, and a footgun that only fires in
//! production is the worst kind.
//!
//! ## What it does not do, stated rather than implied
//!
//! **No TLS here.** The listener is plain HTTP, defaulting to loopback.
//! Exposing it beyond the host is an explicit decision an operator makes,
//! and the supported way is the one they already run — a reverse proxy that
//! terminates TLS, or a private network. meshware's *own* mesh traffic is
//! key-pinned TLS 1.3 over QUIC and always has been; this is a local
//! management socket that happens to speak HTTP, and pretending otherwise
//! by bolting on a second certificate story would be worse than saying so.
//! The envelope is signed either way, so a network attacker cannot forge a
//! request — only read a response.
//!
//! **And it names this node** (SEC-12). A request over HTTP must be signed
//! under `mw-ipc-v2` for this mesh and this node
//! (`Keypair::sign_request_for`), so one read off the wire is refused by
//! every other node rather than good at all of them for five minutes. A
//! request that streams (a run, a fan-out) answers with its first frame —
//! for a run, the job id — and the work goes on; nothing holds a permit
//! past [`ROUTE_TIMEOUT`]. A listener bound beyond loopback says, at every
//! start, that it needs TLS in front of it.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::daemon::Node;
use mesh_proto::SignedIpc;

/// The API version in the path. Bumped only for a breaking change to the
/// envelope contract — never for a new [`IpcRequest`] variant, which is
/// additive and already handled by the protocol's own minor versioning.
pub const API_VERSION: &str = "v1";

/// Largest request body accepted. An envelope carrying a workflow document
/// or a big fan-out selector is kilobytes; this is the ceiling that stops a
/// hostile client making the daemon allocate.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// How long to wait for a client to finish sending. A connection that
/// opens and says nothing costs one task and then goes away.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Requests in flight at once (S-M1).
///
/// The accept loop used to spawn a task per connection with nothing
/// counting them, so "one task and then goes away" was true of each
/// connection and false of a thousand of them: with `READ_TIMEOUT` at
/// thirty seconds, a client that opens sockets and says nothing parks as
/// many tasks as it can open sockets, each holding a buffer and a slot on
/// the runtime the daemon also runs its own work on.
///
/// A semaphore rather than a rate limit, because the thing worth bounding
/// is concurrency and not arrival: a legitimate client making 500 requests
/// a second is fine, and 64 of them stuck mid-request is not. Over the
/// limit, the connection is answered — `503`, immediately, with the code a
/// client can retry on — rather than dropped, so a busy API says so
/// instead of looking broken.
const MAX_INFLIGHT: usize = 64;

pub async fn bind(addr: SocketAddr) -> std::io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        // Said at every start, because it is the one fact about this
        // listener an operator must not forget (SEC-12): requests are
        // signed and bound to this node, but responses cross the network
        // in the clear unless something in front terminates TLS.
        warn!(
            %local,
            "the HTTP API is listening beyond loopback over plain HTTP — put a TLS-terminating \
             proxy in front of it, or keep it on a private network"
        );
    }
    Ok((listener, local))
}

/// How long one request may take to produce its answer before the client
/// is told so (SEC-12). A streamed request answers with its first frame;
/// nothing single-shot here should take this long, and a handler that does
/// is not allowed to hold a permit while it does.
const ROUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub async fn serve(listener: TcpListener, node: Node, mut shutdown: watch::Receiver<bool>) {
    let inflight = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
    loop {
        let conn = tokio::select! {
            c = listener.accept() => c,
            _ = shutdown.changed() => return,
        };
        match conn {
            Ok((stream, peer)) => {
                let node = node.clone();
                // Taken before the task is spawned, so a flood is bounded
                // at the accept loop rather than at the runtime.
                let permit = std::sync::Arc::clone(&inflight).try_acquire_owned().ok();
                tokio::spawn(async move {
                    let Some(permit) = permit else {
                        debug!(%peer, "api at capacity");
                        busy(stream).await;
                        return;
                    };
                    if let Err(e) = handle(stream, node, peer.ip()).await {
                        debug!(%peer, "api request failed: {e}");
                    }
                    drop(permit);
                });
            }
            Err(e) => {
                warn!("api accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// A correlation id, echoed in `X-Meshware-Request-Id` and in errors.
///
/// Not a UUID crate: sixteen hex characters of the same randomness
/// everything else here uses, which is enough to find one request in a log
/// and costs no dependency.
fn request_id() -> String {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        // A correlation id is a convenience, never a security property, so
        // a degraded one beats failing the request.
        b = mesh_proto::now_micros().to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Reply {
    status: &'static str,
    body: String,
}

fn json_error(status: &'static str, code: &str, message: &str, rid: &str) -> Reply {
    // One error shape everywhere, so a client can branch on `code` rather
    // than on prose that will be reworded.
    Reply {
        status,
        body: serde_json::json!({
            "error": { "code": code, "message": message },
            "request_id": rid,
        })
        .to_string(),
    }
}

/// The answer to a connection there was no room for. Written without
/// reading the request: a client at the limit needs the status, not a
/// round trip that would hold the slot it could not get.
async fn busy(mut stream: TcpStream) {
    let rid = request_id();
    let reply = json_error(
        "503 Service Unavailable",
        "too_many_requests",
        "the api has too many requests in flight — retry",
        &rid,
    );
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         X-Meshware-Request-Id: {rid}\r\nRetry-After: 1\r\nCache-Control: no-store\r\n\
         Connection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body,
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn handle(mut stream: TcpStream, node: Node, from: std::net::IpAddr) -> std::io::Result<()> {
    let rid = request_id();
    let reply = match tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(req)) => route(req, node, &rid, from).await,
        Ok(Err(e)) => json_error("400 Bad Request", "malformed_request", &e, &rid),
        Err(_) => json_error(
            "408 Request Timeout",
            "read_timeout",
            "the request was not sent within 30s",
            &rid,
        ),
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         X-Meshware-Request-Id: {rid}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body,
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// A deliberately small HTTP/1.1 reader: request line, headers,
/// `Content-Length` body. No chunked encoding, no keep-alive, no
/// pipelining — the same shape `http.rs` has served since M6, for the same
/// reason (a static musl binary with no web framework in it).
async fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    // Head first.
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 * 1024 {
            return Err("request head too large".into());
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed before the request head ended".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let start = lines.next().unwrap_or_default();
    let mut sp = start.split_whitespace();
    let method = sp.next().unwrap_or_default().to_string();
    let path = sp.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err("could not read the request line".into());
    }

    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = v
                .trim()
                .parse()
                .map_err(|_| "content-length is not a number".to_string())?;
        }
    }
    if content_length > MAX_BODY {
        return Err(format!("body is larger than the {MAX_BODY}-byte ceiling"));
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read body: {e}"))?;
        if n == 0 {
            return Err("connection closed before the body was complete".into());
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > MAX_BODY {
            return Err(format!("body is larger than the {MAX_BODY}-byte ceiling"));
        }
    }
    body.truncate(content_length);
    Ok(Request { method, path, body })
}

async fn route(req: Request, node: Node, rid: &str, from: std::net::IpAddr) -> Reply {
    // Strip a query string; nothing here reads one, and leaving it on would
    // make `/api/v1/version?x=1` a 404 for no reason.
    let path = req.path.split('?').next().unwrap_or("").to_string();
    match (req.method.as_str(), path.as_str()) {
        // Unauthenticated on purpose, and it discloses nothing a port scan
        // does not: which API version this node speaks, so a client can
        // fail with a sentence instead of a parse error.
        ("GET", "/api/version") | ("GET", "/api/v1/version") => Reply {
            status: "200 OK",
            body: serde_json::json!({
                "api": API_VERSION,
                "meshware": env!("CARGO_PKG_VERSION"),
                "protocol_minor": mesh_proto::PROTOCOL_MINOR,
            })
            .to_string(),
        },
        ("POST", "/api/v1/ipc") => ipc(req.body, node, rid, from).await,
        ("GET", _) | ("POST", _) => json_error(
            "404 Not Found",
            "not_found",
            "unknown path — this API has two: GET /api/v1/version and POST /api/v1/ipc",
            rid,
        ),
        _ => json_error(
            "405 Method Not Allowed",
            "method_not_allowed",
            "only GET and POST are served",
            rid,
        ),
    }
}

async fn ipc(body: Vec<u8>, node: Node, rid: &str, from: std::net::IpAddr) -> Reply {
    let env: SignedIpc = match postcard::from_bytes(&body) {
        Ok(e) => e,
        Err(e) => {
            return json_error(
                "400 Bad Request",
                "malformed_envelope",
                &format!(
                    "the body must be a postcard-encoded SignedIpc envelope — the same one the \
                     CLI signs. ({e})"
                ),
                rid,
            );
        }
    };

    // **The whole of the authorization story is this line.** Same dispatch,
    // same envelope verification, same RBAC, same change control, same
    // audit. There is nothing for this module to get wrong because there is
    // nothing for it to decide — except that a caller over HTTP is not on
    // this machine, so the requests that name a path on it are refused
    // (SEC-3).
    let resp = match tokio::time::timeout(ROUTE_TIMEOUT, node.handle_signed_remote(env, from)).await
    {
        Ok(resp) => resp,
        Err(_) => {
            return json_error(
                "504 Gateway Timeout",
                "route_timeout",
                "the node did not answer within 120s",
                rid,
            );
        }
    };

    // The response's own shape decides the status, so a client can use HTTP
    // status codes without meshware inventing a second error vocabulary.
    let status = match &resp {
        mesh_proto::IpcResponse::Denied { .. } => "403 Forbidden",
        // The anonymous doors' limits (SEC-19): a client that backs off on
        // 429 should not have to read the sentence to know to.
        mesh_proto::IpcResponse::Error { message }
            if message.starts_with(crate::throttle::TOO_MANY) =>
        {
            "429 Too Many Requests"
        }
        mesh_proto::IpcResponse::Error { .. } => "400 Bad Request",
        _ => "200 OK",
    };
    match serde_json::to_string(&resp) {
        Ok(json) => Reply {
            status,
            body: format!("{{\"request_id\":\"{rid}\",\"response\":{json}}}"),
        },
        Err(e) => json_error(
            "500 Internal Server Error",
            "encode_failed",
            &format!("could not encode the response: {e}"),
            rid,
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_request_id_is_sixteen_hex_characters() {
        let a = request_id();
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Two in a row must differ, or it is not a correlation id.
        assert_ne!(a, request_id());
    }

    #[test]
    fn an_error_has_one_shape() {
        let r = json_error("403 Forbidden", "denied", "nope", "abcd");
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["error"]["code"], "denied");
        assert_eq!(v["error"]["message"], "nope");
        assert_eq!(v["request_id"], "abcd");
    }
}
