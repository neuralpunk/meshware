//! A deliberately small HTTPS/1.1 client: TCP + tokio-rustls with the
//! platform's root store, keep-alive where the response says exactly where
//! it ends, Content-Length or chunked bodies, a handful of redirects
//! (GitHub release downloads are a 302 to object storage). No HTTP
//! library: the static musl binary stays lean and the dependency tree
//! stays ring-only.
//!
//! Two callers, and they are the whole of meshware's outbound HTTP:
//! `meshnode upgrade --self` fetches a release over [`Client::get`]
//! (MASTERPLAN M7), and a history sink posts batches to its destination
//! over [`Client::post_json`] (M31). One socket implementation rather than
//! two, because the second one would be the same file with a verb changed —
//! and a second TLS setup is a second place to get a certificate verifier
//! wrong.
//!
//! HTTPS only, with one deliberate exception each way: the upgrade module
//! allows a plain `http://` fixture loudly for tests, and a sink allows it
//! for a destination on the operator's **own network**, because a warehouse
//! on your LAN is the ordinary case and refusing it is how a tool teaches
//! people to disable checking everywhere. [`is_private_host`] is that rule,
//! and it lives here so the one definition is shared.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// Redirect hops followed before giving up.
const MAX_REDIRECTS: usize = 5;
/// Connect / TLS handshake budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Whole-response budget (a release binary is ~20 MB; be generous, and a
/// local model on CPU is slower than any download).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
/// Response head cap.
const MAX_HEAD: usize = 64 * 1024;

/// How long a connection may sit unused before it is opened again rather
/// than reused (**H-6**, ENG-27).
///
/// **Short on purpose.** The failure mode of HTTP keep-alive is a server
/// closing an idle connection at the same moment a client writes to it:
/// the write succeeds locally, the request is never processed, and the
/// read returns EOF. There is no way to tell that from a request the
/// server *did* process and then failed to answer — so a POST cannot be
/// replayed, and this client does not. It makes the race rare instead:
/// ten seconds is far inside every default server keep-alive (nginx 75 s,
/// Apache 5 s but with an explicit `Keep-Alive` header, most warehouses
/// 60 s+), so a connection this client reuses is one the server was still
/// holding.
const IDLE_REUSE: Duration = Duration::from_secs(10);

/// Idle connections kept per destination.
const MAX_IDLE_PER_HOST: usize = 4;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The URL the body finally came from (after redirects).
    pub final_url: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
struct Url {
    https: bool,
    host: String,
    port: u16,
    /// Path + query, always starting with `/`.
    path: String,
}

fn parse_url(url: &str) -> Result<Url> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("malformed url {url:?}"))?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => bail!("unsupported url scheme {other:?} in {url:?}"),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = authority
        .split_once('@')
        .map(|(_, a)| a)
        .unwrap_or(authority);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // [v6]:port
        let (h, tail) = stripped
            .split_once(']')
            .ok_or_else(|| anyhow!("malformed host in {url:?}"))?;
        let port = tail
            .strip_prefix(':')
            .map(|p| p.parse::<u16>())
            .transpose()
            .context("bad port")?;
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>().context("bad port")?)),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() {
        bail!("missing host in {url:?}");
    }
    Ok(Url {
        https,
        host,
        port: port.unwrap_or(if https { 443 } else { 80 }),
        path: if path.is_empty() {
            "/".into()
        } else {
            path.to_string()
        },
    })
}

/// Resolve a `Location` header against the request URL.
pub(crate) fn resolve_location(base: &str, location: &str) -> Result<String> {
    if location.contains("://") {
        return Ok(location.to_string());
    }
    let u = parse_url(base)?;
    let scheme = if u.https { "https" } else { "http" };
    let default_port = if u.https { 443 } else { 80 };
    let authority = if u.port == default_port {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    };
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    let dir = match u.path.rfind('/') {
        Some(i) => &u.path[..=i],
        None => "/",
    };
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, data),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The client. Cheap to build; one TLS config shared across requests.
pub struct Client {
    tls: Arc<rustls::ClientConfig>,
    allow_http: bool,
    max_body: usize,
    user_agent: String,
}

/// The TLS configuration, built once for the process (**H-6**).
///
/// `rustls_platform_verifier::Verifier::new` locates and parses the
/// machine's trust store — on Linux that is reading and decoding every
/// certificate in `/etc/ssl` — and it was being done **per request**.
/// `sinks::http_write` constructs a fresh `Client` for every batch it
/// ships, and payload export sends up to a hundred of those per sink per
/// thirty-second tick, so a node with two HTTP sinks re-read its root store
/// a couple of hundred times a minute. Worse, it did so synchronously
/// inside an `async fn`, so each one blocked a reactor thread.
///
/// Nothing about the config varies per caller: `allow_http` is enforced
/// above TLS and `max_body` is a read limit, so one configuration serves
/// every client and sharing it costs an `Arc` clone.
///
/// The error is stored too. A machine with an unreadable trust store fails
/// the same way every time, and retrying the parse per request would only
/// mean paying for the failure repeatedly.
fn shared_tls() -> Result<Arc<rustls::ClientConfig>> {
    static TLS: std::sync::OnceLock<std::result::Result<Arc<rustls::ClientConfig>, String>> =
        std::sync::OnceLock::new();
    TLS.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
            .map_err(|e| format!("platform certificate verifier: {e}"))?;
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls versions: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Arc::new(tls))
    })
    .clone()
    .map_err(|e| anyhow!(e))
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "httpc::Client(allow_http={})", self.allow_http)
    }
}

impl Client {
    /// Build a client verifying servers against the platform root store.
    /// `allow_http` permits plain `http://` URLs (test fixtures only).
    pub fn new(allow_http: bool, max_body: usize) -> Result<Self> {
        Ok(Client {
            tls: shared_tls()?,
            allow_http,
            max_body,
            user_agent: format!("meshware/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    /// GET with redirects. Non-2xx final statuses are errors.
    pub async fn get(&self, url: &str) -> Result<Response> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let resp =
                tokio::time::timeout(RESPONSE_TIMEOUT, self.send_once("GET", &current, &[], None))
                    .await
                    .map_err(|_| anyhow!("GET {current}: timed out"))??;
            match resp.status {
                200..=299 => return Ok(resp),
                301 | 302 | 303 | 307 | 308 => {
                    let location = resp
                        .header("location")
                        .ok_or_else(|| anyhow!("GET {current}: redirect without Location"))?;
                    current = resolve_location(&current, location)?;
                }
                status => bail!("GET {current}: HTTP {status}"),
            }
        }
        bail!("GET {url}: too many redirects")
    }

    /// GET once with headers, following **no** redirects and returning the
    /// response whatever its status.
    ///
    /// Both departures from [`Client::get`] are the same ones `post_json`
    /// makes, for the same reasons. The headers exist because the request
    /// carries an API key, and a key must not be replayed against whatever
    /// host a `Location` names. And a 4xx from a destination carries the
    /// message a person needs — "table does not exist", "not authorised to
    /// SELECT" — so the status and the body both come back and the caller
    /// decides what to print.
    pub async fn get_with(&self, url: &str, headers: &[(String, String)]) -> Result<Response> {
        tokio::time::timeout(RESPONSE_TIMEOUT, self.send_once("GET", url, headers, None))
            .await
            .map_err(|_| anyhow!("GET {url}: timed out"))?
    }

    /// POST a JSON body once, following no redirects and **returning the
    /// response whatever its status**.
    ///
    /// Both departures from `get` are deliberate. A POST that is redirected
    /// is a misconfiguration a caller should hear about verbatim rather
    /// than have replayed against another host — this is a request carrying
    /// an API key. And a 4xx from a model provider carries the *message a
    /// person needs*: "credit balance too low", "no such model". Turning
    /// that into `HTTP 400` throws away the only useful part, so the status
    /// and the body both come back and the caller decides.
    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response> {
        tokio::time::timeout(
            RESPONSE_TIMEOUT,
            self.send_once("POST", url, headers, Some(body)),
        )
        .await
        .map_err(|_| anyhow!("POST {url}: timed out"))?
    }

    async fn send_once(
        &self,
        method: &str,
        url: &str,
        extra_headers: &[(String, String)],
        body_out: Option<&[u8]>,
    ) -> Result<Response> {
        let u = parse_url(url)?;
        if !u.https && !self.allow_http {
            bail!("refusing plain http url {url:?} (HTTPS only)");
        }
        let key: PoolKey = (u.host.clone(), u.port, u.https);
        // An idle connection to this destination, if one is fresh enough
        // (**H-6**, ENG-27). A sink posts a batch every thirty-second tick
        // and used to pay a TCP handshake and a TLS handshake for each.
        let (mut stream, reused) = match take_idle(&key) {
            Some(stream) => (stream, true),
            None => (self.connect(&u).await?, false),
        };
        let mut request = format!(
            "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: */*\r\n",
            u.path,
            host_header(&u),
            self.user_agent
        );
        for (name, value) in extra_headers {
            // A header value with a newline in it is header injection, and
            // these values come from a mesh secret and a config field. The
            // check is one line and closes the whole class.
            if value.contains(['\r', '\n']) || name.contains(['\r', '\n']) {
                bail!("refusing header {name:?} containing a line break");
            }
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = body_out {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");

        // **A failed write on a reused connection is retried; a failed read
        // is not.** A write that fails proves the request was never
        // delivered, so opening a fresh connection and sending it again is
        // safe even for a POST. A write that succeeded and then would not
        // read proves nothing — the server may have processed it — so that
        // surfaces as an error and the sink retries the batch from its
        // spool, which is the one place that knows whether it is safe to.
        // The same rule the CLI's own connection reuse is built on.
        let write = async {
            stream.write_all(request.as_bytes()).await?;
            if let Some(body) = body_out {
                stream.write_all(body).await?;
            }
            stream.flush().await
        }
        .await;
        if let Err(e) = write {
            if !reused {
                return Err(e).with_context(|| format!("{method} {url}: sending the request"));
            }
            stream = self.connect(&u).await?;
            stream.write_all(request.as_bytes()).await?;
            if let Some(body) = body_out {
                stream.write_all(body).await?;
            }
            stream.flush().await?;
        }

        // Head.
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let head_end = loop {
            let mut chunk = [0u8; 8192];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("{method} {url}: connection closed before response head");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = find_head_end(&buf) {
                break i;
            }
            if buf.len() > MAX_HEAD {
                bail!("{method} {url}: response head too large");
            }
        };
        let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
        let (status, headers) =
            parse_head(&buf[..head_end]).map_err(|e| anyhow!("{method} {url}: {e}"))?;
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };

        let chunked =
            header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let content_length: Option<usize> = header("content-length").and_then(|v| v.parse().ok());
        let no_body = matches!(status, 204 | 304) || (100..200).contains(&status);

        // Decided here, from the headers, before a byte of body is read:
        // whether this connection will be at a known boundary afterwards.
        let keep_alive = !header("connection")
            .is_some_and(|v| v.to_ascii_lowercase().contains("close"))
            && buf.starts_with(b"HTTP/1.1");
        let mut reuse = Reuse::No;

        let body = if no_body {
            // 204/304/1xx carry no body at all, so the connection is at a
            // boundary the moment the head ends — provided nothing else
            // arrived with it.
            if keep_alive && body.is_empty() {
                reuse = Reuse::AtBoundary;
            }
            Vec::new()
        } else if chunked {
            // Read everything, then dechunk.
            self.read_to_end(&mut stream, &mut body, url).await?;
            dechunk(&body).with_context(|| format!("{method} {url}: bad chunked body"))?
        } else if let Some(len) = content_length {
            if len > self.max_body {
                bail!(
                    "{method} {url}: body of {len} bytes exceeds the {} byte cap",
                    self.max_body
                );
            }
            while body.len() < len {
                let mut chunk = vec![0u8; (len - body.len()).min(64 * 1024)];
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    bail!(
                        "{method} {url}: connection closed mid-body ({} of {len} bytes)",
                        body.len()
                    );
                }
                body.extend_from_slice(&chunk[..n]);
            }
            // Exactly `len` bytes were read and the reader stopped there.
            // This is the only state in which the next request can safely
            // go down this socket — see [`Reuse`].
            if keep_alive && body.len() == len {
                reuse = Reuse::AtBoundary;
            }
            body.truncate(len);
            body
        } else {
            // EOF-delimited: reading the body *is* consuming the
            // connection, so there is nothing to put back.
            self.read_to_end(&mut stream, &mut body, url).await?;
            body
        };
        if reuse.allows() {
            put_idle(key, stream);
        }
        Ok(Response {
            status,
            headers,
            body,
            final_url: url.to_string(),
        })
    }

    /// One fresh connection to `u`, TLS included.
    async fn connect(&self, u: &Url) -> Result<Stream> {
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((u.host.as_str(), u.port)),
        )
        .await
        .map_err(|_| anyhow!("connect {}:{}: timed out", u.host, u.port))?
        .with_context(|| format!("connect {}:{}", u.host, u.port))?;
        if !u.https {
            return Ok(Stream::Plain(tcp));
        }
        let name = ServerName::try_from(u.host.clone())
            .map_err(|_| anyhow!("invalid server name {:?}", u.host))?;
        let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
        let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
            .await
            .map_err(|_| anyhow!("tls handshake with {}: timed out", u.host))?
            .with_context(|| format!("tls handshake with {}", u.host))?;
        Ok(Stream::Tls(Box::new(tls)))
    }

    async fn read_to_end(&self, stream: &mut Stream, body: &mut Vec<u8>, url: &str) -> Result<()> {
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            body.extend_from_slice(&chunk[..n]);
            if body.len() > self.max_body {
                bail!("GET {url}: body exceeds the {} byte cap", self.max_body);
            }
        }
    }
}

/// Where a destination's idle connections wait (**H-6**, ENG-27).
///
/// Process-wide rather than per-[`Client`], because `sinks::http_write`
/// builds a fresh `Client` for every batch it ships — a pool inside one
/// would be empty every time and the connection count would not change.
/// The same reasoning `shared_tls` is built on, one layer out.
///
/// Keyed by what actually determines whether a socket can carry the next
/// request: the host, the port, and whether it is TLS. Not by the path,
/// and not by the headers — a keep-alive connection is to a *server*.
type PoolKey = (String, u16, bool);

struct Idle {
    stream: Stream,
    since: std::time::Instant,
}

fn pool() -> &'static std::sync::Mutex<std::collections::HashMap<PoolKey, Vec<Idle>>> {
    static POOL: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PoolKey, Vec<Idle>>>,
    > = std::sync::OnceLock::new();
    POOL.get_or_init(Default::default)
}

/// Take an idle connection for `key`, if one is fresh enough to trust.
fn take_idle(key: &PoolKey) -> Option<Stream> {
    let mut pool = pool().lock().ok()?;
    let entry = pool.get_mut(key)?;
    // The newest one: the least likely to have been closed under us.
    let idle = entry.pop()?;
    if idle.since.elapsed() < IDLE_REUSE {
        return Some(idle.stream);
    }
    // Pushed in order, so everything under it is older still — none of it
    // is worth trying and all of it is worth dropping.
    entry.clear();
    None
}

/// Put a connection back, if it is at a known boundary. See
/// [`Reuse::allows`] for what that means and why it is checked.
fn put_idle(key: PoolKey, stream: Stream) {
    let Ok(mut pool) = pool().lock() else { return };
    let entry = pool.entry(key).or_default();
    if entry.len() >= MAX_IDLE_PER_HOST {
        return;
    }
    entry.push(Idle {
        stream,
        since: std::time::Instant::now(),
    });
}

/// Whether the connection a response arrived on may carry another request.
///
/// **The only safe answer is "the body ended exactly where the headers
/// said it would".** A response read to EOF has, by definition, consumed
/// the connection. A chunked one ends at a terminator this client reads
/// past into whatever follows. A `Content-Length` body is the one case
/// where the client knows, byte for byte, that it stopped at the boundary
/// — and reusing a connection in any other state means the *next* request
/// reads the tail of this one's body as its own reply. On the path that
/// ships a mesh's history to somebody's warehouse, that is not a
/// performance bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reuse {
    /// Exactly `Content-Length` bytes were read, and nothing said close.
    AtBoundary,
    No,
}

impl Reuse {
    fn allows(self) -> bool {
        self == Reuse::AtBoundary
    }
}

/// A response head — status line and headers — as a server sent it. Pure,
/// so the part of this client a server controls can be fuzzed (SEC-22).
pub(crate) fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>)> {
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad status line {status_line:?}"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok((status, headers))
}

fn host_header(u: &Url) -> String {
    let default_port = if u.https { 443 } else { 80 };
    if u.port == default_port {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Decode a `Transfer-Encoding: chunked` body.
pub(crate) fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("truncated chunk size"))?;
        let size_str = std::str::from_utf8(&data[..line_end])?;
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).context("bad chunk size")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        // **Checked** (S-M6). `size` is a `usize` parsed from hex the server
        // chose, so `size + 2` wrapped: at `usize::MAX - 1` it came out as
        // `1`, the length guard passed, and `&data[..size]` panicked on an
        // out-of-bounds slice — release builds have overflow checks off, so
        // the wrap was silent. Reachable from the release server
        // (`meshnode upgrade --self`), the drafting agent's model endpoint,
        // and every sink destination.
        let need = size
            .checked_add(2)
            .ok_or_else(|| anyhow!("chunk size out of range"))?;
        if data.len() < need {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[need..];
    }
}

/// The host part of a base URL, with any `[…]` v6 brackets stripped.
///
/// Lived in `agent.rs` until the drafting agent was removed. It was always
/// a URL helper rather than anything to do with models, and the two paths
/// that still share the rule below — a history sink and `meshnode`'s
/// job-finished webhook — both reach outbound HTTP through this module. One
/// definition of "is this host on your own network" is the point, and this
/// is where it belongs.
pub fn host_of(base: &str) -> &str {
    let after = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let authority = after.split('/').next().unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    authority.split(':').next().unwrap_or("")
}

/// Whether a host is on the operator's own network, and therefore whether
/// plain `http://` to it is acceptable.
///
/// Plain HTTP is allowed to private space — loopback, RFC1918, link-local,
/// v6 unique-local — and refused to anything routable, where the payload
/// would cross the internet in the clear along with whatever credential the
/// headers carry. A bare single-label name and `*.local` are allowed on the
/// same reasoning: those resolve on a LAN, and a public service is never
/// reached at `warehouse-box`.
///
/// **Allowing a LAN is the load-bearing half.** Refusing it would make
/// "nothing leaves the building" a slogan the product did not support, and
/// the workaround people reach for when a tool refuses their own network is
/// a flag that disables checking everywhere.
pub fn is_private_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                let first = v6.segments()[0];
                // `is_unique_local` and `is_unicast_link_local` are still
                // unstable, so the two prefixes are spelled out: fc00::/7
                // and fe80::/10.
                v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
            }
        };
    }
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".local") || !host.contains('.')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A server that answers `replies` in order on however many
    /// connections a client opens, counting them.
    ///
    /// Written out by hand because the whole question is *how many TCP
    /// connections the client made*, which is precisely what a mock HTTP
    /// client would hide.
    async fn serve(
        replies: Vec<&'static str>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            let mut next = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                count.fetch_add(1, Ordering::Relaxed);
                let replies = replies.clone();
                tokio::spawn(async move {
                    // One request at a time, for as long as the client
                    // keeps the connection: read a head, answer it.
                    loop {
                        let mut buf = Vec::new();
                        let mut byte = [0u8; 1];
                        while find_head_end(&buf).is_none() {
                            match sock.read(&mut byte).await {
                                Ok(0) | Err(_) => return,
                                Ok(_) => buf.extend_from_slice(&byte),
                            }
                        }
                        // Any request body: read exactly what was declared.
                        let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                        let len: usize = head
                            .split("\r\n")
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let mut body = vec![0u8; len];
                        if len > 0 && sock.read_exact(&mut body).await.is_err() {
                            return;
                        }
                        let reply = replies[next.min(replies.len() - 1)];
                        if sock.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                        next += 1;
                        // Close unless the reply carried a `Content-Length`.
                        //
                        // Not a simplification of a real server — it is
                        // what *this* client needs. It reads a chunked body
                        // to EOF and dechunks afterwards rather than
                        // stopping at the terminator, so a server holding a
                        // chunked connection open would hang it. Which is
                        // the same fact that makes a chunked response
                        // un-poolable, seen from the other side.
                        let lower = reply.to_ascii_lowercase();
                        if lower.contains("connection: close") || !lower.contains("content-length:")
                        {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), accepted)
    }

    /// **H-6's remainder** (ENG-27): a sink posting every tick paid a TCP
    /// handshake, and on a real destination a TLS handshake, for each
    /// batch. A `Content-Length` response leaves the connection at a known
    /// boundary, and the next batch goes down the same socket.
    #[tokio::test]
    async fn a_content_length_response_leaves_the_connection_reusable() {
        let (base, accepted) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ])
        .await;
        let client = Client::new(true, 1 << 20).unwrap();
        for _ in 0..3 {
            let r = client
                .post_json(&format!("{base}/rows"), &[], b"[]")
                .await
                .unwrap();
            assert_eq!(r.status, 200);
            assert_eq!(r.body, b"ok");
        }
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "three batches should share one connection"
        );
    }

    /// …and a response this client cannot prove it stopped at the end of
    /// **is not** reused, however tempting.
    ///
    /// A chunked body is read past its terminator into whatever follows,
    /// and an EOF-delimited one consumes the connection by definition.
    /// Reusing either means the next batch reads the tail of this one's
    /// body as its own reply — which, on the path that ships a mesh's
    /// history to somebody's warehouse, is not a performance bug.
    #[tokio::test]
    async fn a_response_without_a_known_end_is_never_reused() {
        for reply in [
            // Says close, whatever else it says.
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            // Chunked: ends at a terminator, not at a counted byte.
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            // Neither: the body is "until the connection closes".
            "HTTP/1.1 200 OK\r\n\r\nok",
        ] {
            let (base, accepted) = serve(vec![reply, reply]).await;
            let client = Client::new(true, 1 << 20).unwrap();
            for _ in 0..2 {
                let _ = client.post_json(&format!("{base}/rows"), &[], b"[]").await;
            }
            assert_eq!(
                accepted.load(std::sync::atomic::Ordering::Relaxed),
                2,
                "reused a connection after {reply:?}"
            );
        }
    }

    #[test]
    fn url_parsing() {
        let u = parse_url("https://api.github.com/repos/x/y/releases/latest").unwrap();
        assert!(u.https);
        assert_eq!(u.host, "api.github.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/repos/x/y/releases/latest");
        let u = parse_url("http://127.0.0.1:8080").unwrap();
        assert!(!u.https);
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
        assert!(parse_url("ftp://x/y").is_err());
        assert_eq!(
            resolve_location("https://a.example/x/y", "/z").unwrap(),
            "https://a.example/z"
        );
        assert_eq!(
            resolve_location("https://a.example/x/y", "w").unwrap(),
            "https://a.example/x/w"
        );
        assert_eq!(
            resolve_location("http://a.example:81/x", "https://b.example/q").unwrap(),
            "https://b.example/q"
        );
    }

    /// S-M6: an absurd chunk size is an error, never an arithmetic wrap.
    ///
    /// `size + 2` at `usize::MAX - 1` came out as `1`, which passed the
    /// length guard and let `&data[..size]` slice past the end. Release
    /// builds have overflow checks off, so nothing said so first.
    #[test]
    fn an_enormous_chunk_size_is_refused_rather_than_wrapping() {
        let max = format!("{:x}", usize::MAX);
        let body = format!("{max}\r\nmeshware\r\n0\r\n\r\n");
        assert!(dechunk(body.as_bytes()).is_err());
        let near = format!("{:x}", usize::MAX - 1);
        let body = format!("{near}\r\nmeshware\r\n0\r\n\r\n");
        assert!(dechunk(body.as_bytes()).is_err());
        // And one that is merely bigger than what arrived.
        assert!(dechunk(b"ffffffff\r\nmeshware\r\n").is_err());
    }

    #[test]
    fn dechunk_decodes() {
        let body = b"4\r\nmesh\r\n4;ext=1\r\nware\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).unwrap(), b"meshware");
        assert!(dechunk(b"4\r\nme").is_err());
    }

    #[test]
    fn the_host_is_read_out_of_any_shape_of_base_url() {
        assert_eq!(host_of("http://192.168.2.105:11434"), "192.168.2.105");
        assert_eq!(host_of("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(host_of("http://[fd00::1]:11434/x"), "fd00::1");
        assert_eq!(host_of("http://gpu.local"), "gpu.local");
    }

    /// The rule that decides whether somebody's own box on their own LAN is
    /// reachable without TLS. Moved here with the function; the callers are
    /// now a history sink and the job-finished webhook.
    #[test]
    fn a_destination_on_your_own_network_counts_as_private() {
        for host in [
            "192.168.2.105",
            "127.0.0.1",
            "10.0.0.9",
            "172.16.4.4",
            "fd00::1",
            "::1",
            "169.254.1.1",
            "warehouse-box",
            "clickhouse.local",
            "localhost",
        ] {
            assert!(is_private_host(host), "{host} should count as private");
        }
        // Routable stays public: that is where a batch carrying this mesh's
        // whole history would cross the internet in the clear.
        for host in ["api.example.com", "8.8.8.8", "2001:4860:4860::8888"] {
            assert!(!is_private_host(host), "{host} should count as public");
        }
    }
}
