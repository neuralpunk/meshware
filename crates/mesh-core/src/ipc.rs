//! Local IPC between the daemon and its frontends (CLI, Director) over a
//! Unix domain socket, carrying the same framed postcard schema as the wire
//! (MASTERPLAN §6, "Local IPC").
//!
//! Every frame from a client is a [`SignedIpc`] envelope — the daemon
//! authenticates the *user*, not the socket (MASTERPLAN §7.1). Most
//! requests get one response frame; `Run`, `RunService` and `JobLogs`
//! stream many frames (JobStarted / LogChunk* / JobDone) before the next
//! request is read.
//!
//! **One request is not like the others.** `SessionOpen` (M17 §3.3) ends
//! the request/response loop and hands the whole socket to the session
//! machinery, which thereafter speaks `SessionFrame` client→daemon and
//! `SessionServerFrame` daemon→client. This is the local half of R4's
//! problem — the socket is one-shot in exactly the way the wire is, so an
//! interactive feature has to solve it on two legs, not one — and it is
//! recognised *here*, before dispatch, because the handler needs both
//! halves and an `mpsc::Sender<IpcResponse>` can only carry one.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::daemon::Node;
use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, SignedIpc, read_frame, write_frame};

pub async fn serve(socket_path: PathBuf, node: Node) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        // **The directory, before the socket** (S-L1). `bind` creates the
        // socket with `0777 & ~umask` and takes no mode argument, so between
        // the bind and the `chmod` below there was a window in which any
        // local user could connect — and this socket is the daemon's whole
        // IPC surface. A `0700` parent closes it without a window at all: a
        // directory nobody else may traverse holds a socket nobody else can
        // reach, whatever mode the socket itself briefly had.
        //
        // Narrowing the process umask around the bind would have worked too
        // and is the wrong tool — `umask` is process-wide, and a library
        // function that changes it for a moment changes it for every other
        // thread creating a file in that moment.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| CoreError::Ipc(format!("bind {}: {e}", socket_path.display())))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    debug!("ipc listening on {}", socket_path.display());
    write_socket_pointer(&node.config().layout().run_dir(), &socket_path);

    // An accept error is almost always a resource running out — file
    // descriptors, most often — and it passes. It used to end this loop,
    // leaving a node the mesh saw as healthy that no CLI, SDK or Director
    // could reach (ENG-2). Back off and try again, as the HTTP listener
    // does, doubling up to a second so a descriptor shortage is not also a
    // busy loop.
    let mut backoff = ACCEPT_BACKOFF_MIN;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => {
                backoff = ACCEPT_BACKOFF_MIN;
                stream
            }
            Err(e) => {
                warn!("ipc accept failed, retrying in {backoff:?}: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                continue;
            }
        };
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, node).await {
                warn!("ipc connection error: {e}");
            }
        });
    }
}

const ACCEPT_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_millis(50);
const ACCEPT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// Name of the pointer file the daemon drops beside its run directory,
/// holding the path of the socket it is actually listening on.
///
/// **The whole reason it exists is C6.** The socket path is derived from a
/// blake3 hash of the canonicalized home under `$XDG_RUNTIME_DIR`, which is
/// a fine scheme for two Rust binaries that share a crate and an impossible
/// one for a client that does not — a client in another language cannot be asked to
/// carry a hash implementation to find a socket. So the daemon writes down
/// where it is, once, at a path any client can compute with `os.path.join`.
///
/// Not a lock file and not a source of truth: the socket is. A stale
/// pointer left by a crashed daemon fails to connect exactly as a stale
/// socket does, and is overwritten by the next start.
pub const SOCKET_POINTER: &str = "socket";

/// Record where this daemon is listening, best-effort.
///
/// A failure here is logged and ignored. A node that cannot write a
/// convenience file for a client that may never connect must still come up:
/// the CLI and Director both derive the path themselves and neither reads
/// this.
fn write_socket_pointer(run_dir: &Path, socket_path: &Path) {
    // In the node's *home*, never beside the socket. Under
    // `$XDG_RUNTIME_DIR` several nodes on one host share one socket
    // directory and are told apart by a hash in the filename — which is
    // precisely the hash a client cannot compute. The home is what a client
    // already knows (`$MESHWARE_HOME`, or `~/.meshware`), and it is one per
    // node by construction.
    if let Err(e) = std::fs::create_dir_all(run_dir) {
        debug!("could not create the run dir: {e}");
        return;
    }
    if let Err(e) = std::fs::write(
        run_dir.join(SOCKET_POINTER),
        socket_path.to_string_lossy().as_bytes(),
    ) {
        debug!("could not write the socket pointer: {e}");
    }
}

async fn handle_conn(mut stream: UnixStream, node: Node) -> Result<()> {
    loop {
        let envelope: SignedIpc = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(mesh_proto::ProtoError::Io(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Peeked, not verified: the daemon verifies the envelope on the
        // other side of this call, and all this decides is *which shape*
        // the rest of the connection has. A forged payload therefore buys
        // nothing but a refusal in a different frame type.
        if is_session_open(&envelope) {
            node.serve_ipc_session(envelope, stream).await;
            return Ok(());
        }
        let (tx, mut rx) = mpsc::channel::<IpcResponse>(64);
        let node2 = node.clone();
        tokio::spawn(async move {
            node2.handle_signed_stream(envelope, tx).await;
        });
        while let Some(resp) = rx.recv().await {
            write_frame(&mut stream, &resp).await?;
        }
    }
}

/// Does this envelope carry a session request?
///
/// **A five-byte varint peek, not a full decode** (**M-16**). This ran
/// `postcard::from_bytes::<IpcRequest>` over the whole payload purely to
/// read one discriminant, and the dispatch below then decoded it a second
/// time — so every request was deserialised twice. For the bulk variants
/// (`data: Vec<u8>` on the content verbs, a megabyte of inline script on a
/// submission) that doubled peak allocation, and it did so *before* the
/// signature had been checked: an unauthenticated caller could make the
/// daemon allocate and parse the payload twice over.
///
/// The discriminant is the first LEB128 varint of a postcard enum, which
/// is exactly what `op_body_index` already reads for ops.
fn is_session_open(envelope: &SignedIpc) -> bool {
    mesh_proto::ipc_request_index(&envelope.payload) == Some(mesh_proto::IPCREQ_SESSION_OPEN_INDEX)
}

/// Open a session against the local daemon and keep **both** halves.
///
/// The client mirror of `Transport::open_session`: write the signed
/// request and then stop treating the socket as a request/response
/// channel. Everything after this is [`mesh_proto::SessionFrame`] out and
/// [`mesh_proto::SessionServerFrame`] in.
pub async fn open_session(
    socket_path: &Path,
    user: &Keypair,
    node: &str,
    kind: mesh_proto::SessionKind,
    reason: &str,
) -> Result<UnixStream> {
    let req = session_request(node, kind, reason);
    send_signed(socket_path, &user.sign_request(&req)?).await
}

/// The request [`open_session`] sends, for a caller that signs it itself.
pub fn session_request(node: &str, kind: mesh_proto::SessionKind, reason: &str) -> IpcRequest {
    IpcRequest::SessionOpen {
        node: node.to_string(),
        kind,
        reason: reason.to_string(),
    }
}

/// Connect and write one envelope that is already signed; the caller reads
/// whatever comes back.
async fn send_signed(socket_path: &Path, envelope: &SignedIpc) -> Result<UnixStream> {
    let mut stream = connect(socket_path).await?;
    write_frame(&mut stream, envelope).await?;
    Ok(stream)
}

/// One-shot IPC client used by the CLI for non-streaming requests. Signs
/// the request with the caller's user key.
pub async fn request(socket_path: &Path, user: &Keypair, req: IpcRequest) -> Result<IpcResponse> {
    let mut stream = send_signed(socket_path, &user.sign_request(&req)?).await?;
    let resp = read_frame(&mut stream).await?;
    Ok(resp)
}

/// Open an IPC stream and send one signed streaming request; the caller
/// reads frames until the stream terminates.
pub async fn request_stream(
    socket_path: &Path,
    user: &Keypair,
    req: IpcRequest,
) -> Result<UnixStream> {
    send_signed(socket_path, &user.sign_request(&req)?).await
}

/// A frontend's view of one node: its socket, and the key that signs.
///
/// **The one client**, for the CLI's single requests and for Director's
/// hundreds. Director used to embed the node it showed and call it in
/// process; it now talks to this machine's node exactly as `meshnode` does,
/// through this, so the two cannot drift into different answers about the
/// same request.
#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
    user: Keypair,
}

/// The socket, and whose key — never the key.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("socket", &self.socket)
            .field("user", &self.user.user_id().to_display_string())
            .finish()
    }
}

impl Client {
    pub fn new(socket: PathBuf, user: Keypair) -> Client {
        Client { socket, user }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn user(&self) -> &Keypair {
        &self.user
    }

    /// One signed request, one response, as the node sent it — `Error` and
    /// `Denied` included, for the callers that read them.
    ///
    /// **Bounded** by [`deadline_for`]: a node that accepts the connection
    /// and then stalls must not hang whatever is waiting on it for ever.
    pub async fn request(&self, req: IpcRequest) -> Result<IpcResponse> {
        let envelope = self.user.sign_request(&req)?;
        self.request_signed(&req, envelope).await
    }

    /// [`Self::request`] with the envelope already signed, for a caller
    /// that signs somewhere of its own choosing. Director does: a key on a
    /// security key may wait for a person, so it signs off the async
    /// workers, one signature at a time, and sends when it has one. `req`
    /// is what the envelope carries; it sets the deadline.
    pub async fn request_signed(
        &self,
        req: &IpcRequest,
        envelope: SignedIpc,
    ) -> Result<IpcResponse> {
        let secs = deadline_for(req);
        let exchange = async {
            let mut stream = send_signed(&self.socket, &envelope).await?;
            read_frame(&mut stream).await.map_err(CoreError::from)
        };
        match tokio::time::timeout(std::time::Duration::from_secs(secs), exchange).await {
            Ok(resp) => resp,
            Err(_) => Err(CoreError::Ipc(format!(
                "the node accepted the connection but did not answer within {secs}s. It may \
                 be wedged — `meshnode doctor` says more."
            ))),
        }
    }

    /// One signed request whose answer is many frames.
    ///
    /// **The stream ends when the node has nothing more to say,** without
    /// the caller having to know which frame is the last one for this
    /// request. The daemon serves a connection as request, frames, request,
    /// frames …, and only reads the next request once every frame of this
    /// one is written. So this closes its *sending* half straight after the
    /// request: the daemon writes everything, reads for the next request,
    /// finds end-of-file, and hangs up — which is the end of the channel
    /// here. No marker frame, so nothing about the protocol changes for
    /// any other client.
    ///
    /// Dropping the receiver drops the connection, which is how a follow
    /// the caller has lost interest in stops costing the node anything.
    pub async fn stream(&self, req: IpcRequest) -> Result<mpsc::Receiver<IpcResponse>> {
        let envelope = self.user.sign_request(&req)?;
        self.stream_signed(envelope).await
    }

    /// [`Self::stream`] with the envelope already signed; see
    /// [`Self::request_signed`].
    pub async fn stream_signed(&self, envelope: SignedIpc) -> Result<mpsc::Receiver<IpcResponse>> {
        use tokio::io::AsyncWriteExt as _;
        let mut stream = send_signed(&self.socket, &envelope).await?;
        stream.shutdown().await?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = read_frame::<_, IpcResponse>(&mut stream) => match frame {
                        Ok(frame) => {
                            if tx.send(frame).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    () = tx.closed() => return,
                }
            }
        });
        Ok(rx)
    }

    /// Open a session (a terminal, an exec, a file copy) and keep both
    /// halves; see [`open_session`].
    pub async fn open_session(
        &self,
        node: &str,
        kind: mesh_proto::SessionKind,
        reason: &str,
    ) -> Result<UnixStream> {
        open_session(&self.socket, &self.user, node, kind, reason).await
    }

    /// [`Self::open_session`] with the envelope already signed — sign
    /// [`session_request`]; see [`Self::request_signed`].
    pub async fn open_session_signed(&self, envelope: SignedIpc) -> Result<UnixStream> {
        send_signed(&self.socket, &envelope).await
    }
}

/// Requests that legitimately take minutes, not seconds (**M-14**).
///
/// Each one either moves bytes across the network, drives every node in the
/// fleet, or waits for running work to end — the daemon's own deadlines for
/// these are measured in hundreds of seconds (`PREFETCH_DEADLINE` is 900,
/// `UPGRADE_NODE_DEADLINE` 600). A thirty-second client deadline would
/// abort them while they were working correctly.
pub fn slow_request(req: &IpcRequest) -> bool {
    matches!(
        req,
        IpcRequest::Push { .. }
            | IpcRequest::BundlePrefetch { .. }
            | IpcRequest::UpgradeFleet { .. }
            | IpcRequest::SourceSyncNow { .. }
            | IpcRequest::Backup { .. }
            | IpcRequest::Drain { .. }
            | IpcRequest::DrainDeadline { .. }
            | IpcRequest::DrainTimed { .. }
            | IpcRequest::StateCompact { .. }
            | IpcRequest::MeshReset { .. }
    )
}

/// How long a frontend waits for the node to answer `req`, in seconds.
///
/// Thirty for anything a person is watching a spinner for; half an hour
/// for the requests that copy, compact or wait on other machines. The CLI's
/// `--ipc-timeout` overrides it; nothing else should need to.
pub fn deadline_for(req: &IpcRequest) -> u64 {
    if slow_request(req) { 1800 } else { 30 }
}

/// Open an IPC stream for a streaming request; the caller reads frames until
/// the stream terminates.
pub async fn connect(socket_path: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket_path).await.map_err(|e| {
        CoreError::Ipc(match e.kind() {
            // A node *is* there; it just will not talk to this account.
            // Saying "is `meshnode up` running?" to someone looking at a
            // running daemon is the least useful sentence available — and
            // leading with the socket's uid is barely better, because the
            // person reading this is nearly always someone who is simply
            // not on the mesh, not an administrator who mistyped `sudo`.
            // So: the plain fact first, the way in second, and the
            // become-the-daemon-user route last, where it belongs.
            std::io::ErrorKind::PermissionDenied => format!(
                "you are not authorized to use the meshware node on this machine.\n\n\
                 A node is listening at {}, but it runs under another account{} and will \
                 not accept a connection from yours.\n\n\
                 Meshware recognises people by key, not by login. To get access, ask an \
                 administrator of this mesh to add yours:\n  \
                 meshnode whoami   shows your key and where it is kept\n  \
                 meshnode enrol    sends them the request\n\n\
                 If you administer this machine, the CLI can be run as the node's own \
                 account instead: sudo -u <that user> meshnode …",
                socket_path.display(),
                socket_owner(socket_path)
                    .map(|uid| format!(" (uid {uid})"))
                    .unwrap_or_default(),
            ),
            _ => format!(
                "cannot reach daemon at {} — is `meshnode up` running? ({e})",
                socket_path.display()
            ),
        })
    })
}

/// Owning uid of a socket, for an error that can name who to become.
fn socket_owner(socket_path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(socket_path).ok().map(|m| m.uid())
}

// -- finding the node to talk to ----------------------------------------
//
// A client that has to be *told* where the node is has already failed the
// person who typed `meshnode status` on their own workstation. The socket
// path is a hash of a home ([`crate::config::socket_path_for`]), and a
// desktop has two entirely reasonable homes: `~/.meshware` for a daemon,
// and `<home>/director` for the node Director embeds and serves this same
// socket from ("API parity", `apps/director/src-tauri/src/lib.rs`). Neither
// is more correct than the other, and the person typing has no reason to
// know either exists — so the CLI looks instead of guessing once.

// Where a packaged install puts a system daemon
// (`packaging/systemd/meshnode.service`); one constant, in `layout`.
use crate::layout::SYSTEM_HOME;

/// What a candidate's socket had to say when we knocked.
enum Knock {
    /// A daemon accepted the connection.
    Live,
    /// Something is listening and will not let *this* user in — a system
    /// daemon running as its own user, nearly always.
    Forbidden,
    /// Nothing: no socket, or one a dead daemon left behind.
    Silent,
}

/// Knock on a socket.
///
/// Existence proves nothing — a unix socket outlives the process that bound
/// it — so this connects, which is the only honest test. The daemon sees a
/// connection that closes before sending a frame, which [`handle_conn`]
/// already ends cleanly on its `ProtoError::Io` arm: probing is silent.
fn knock(socket_path: &Path) -> Knock {
    match std::os::unix::net::UnixStream::connect(socket_path) {
        Ok(_) => Knock::Live,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Knock::Forbidden,
        Err(_) => Knock::Silent,
    }
}

/// Every path a node with this home could be listening on, best evidence
/// first.
///
/// The pointer file wins when it exists, because it is the only record of
/// where the daemon *actually* bound. That matters for the case the
/// derivation cannot reach: a system daemon has no `XDG_RUNTIME_DIR` and
/// falls back into its own home, while the shell asking about it has one,
/// so the two derive different paths from the same home and neither is
/// wrong.
fn sockets_for(home: &Path) -> Vec<PathBuf> {
    let run = crate::layout::Layout::new(home).run_dir();
    let mut paths = Vec::new();
    if let Ok(pointer) = std::fs::read_to_string(run.join(SOCKET_POINTER)) {
        let pointer = pointer.trim();
        if !pointer.is_empty() {
            paths.push(PathBuf::from(pointer));
        }
    }
    for path in [
        crate::config::socket_path_for(home),
        run.join("meshnode.sock"),
    ] {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// The homes worth looking in, in the order a person would guess.
fn candidate_homes(default_home: &Path) -> Vec<PathBuf> {
    let mut homes = vec![default_home.to_path_buf()];
    // Director keeps its node in `<meshware home>/director` deliberately,
    // so that a standalone daemon and the desktop app never share an
    // identity or an op-log. A sibling home made by hand is found the same
    // way; `identity/` is what makes a directory a home rather than one of
    // the six the layout puts *inside* one.
    if let Ok(entries) = std::fs::read_dir(default_home) {
        let mut nested: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("identity").is_dir())
            .collect();
        nested.sort();
        homes.extend(nested);
    }
    let system = PathBuf::from(SYSTEM_HOME);
    if !homes.contains(&system) {
        homes.push(system);
    }
    homes
}

/// Find the node this machine is actually running, for a caller who did not
/// say which one.
///
/// Returns a *home*, not a socket, and that is the whole point: the home
/// decides the user key as well as the path
/// (`identity::load_cli_user`). Signing with `~/.meshware/identity/user.key`
/// against Director's node earns a refusal rather than an answer — they are
/// different identities on purpose — so redirecting the socket alone would
/// trade a clear "nothing is listening" for a baffling "permission denied".
///
/// `Err` carries the homes that were looked in, so the failure can say
/// where it looked instead of naming one path the person never chose.
///
/// Two live nodes resolve to the first rather than an error: the order is
/// deliberate, and someone running two can still say `--data-dir`. Refusing
/// to act because a machine is well equipped is the behaviour this function
/// exists to delete.
pub fn find_node_home(default_home: &Path) -> std::result::Result<PathBuf, Vec<PathBuf>> {
    let candidates = candidate_homes(default_home);
    let mut forbidden = None;
    for home in &candidates {
        for socket in sockets_for(home) {
            match knock(&socket) {
                Knock::Live => return Ok(home.clone()),
                Knock::Forbidden if forbidden.is_none() => forbidden = Some(home.clone()),
                Knock::Forbidden | Knock::Silent => {}
            }
        }
    }
    // A node that is running but will not talk to this user is still the
    // answer to "where is it". Returning it lets the connection fail with
    // the kernel's own EACCES — which names the socket and its owner —
    // rather than reporting that nothing was found.
    forbidden.ok_or(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node home, optionally with something actually listening on the
    /// socket its pointer file names.
    ///
    /// Driven through the pointer rather than the derivation so the test
    /// does not depend on whether whatever runs it has `$XDG_RUNTIME_DIR`.
    fn home_with(root: &Path, listening: bool) -> Option<std::os::unix::net::UnixListener> {
        std::fs::create_dir_all(root.join("identity")).expect("identity dir");
        let run = root.join("run");
        std::fs::create_dir_all(&run).expect("run dir");
        let socket = run.join("meshnode.sock");
        std::fs::write(
            run.join(SOCKET_POINTER),
            socket.to_string_lossy().as_bytes(),
        )
        .expect("pointer");
        listening.then(|| std::os::unix::net::UnixListener::bind(&socket).expect("bind"))
    }

    /// A refusal an unauthorized person can act on.
    ///
    /// The failing shape in the field is a packaged node running as its own
    /// system user: something *is* listening, and the kernel says EACCES.
    /// What that means to the human at the keyboard is "you are not on this
    /// mesh", and the message has to lead with that — not with a uid, and
    /// certainly not with `is meshnode up running?`.
    #[tokio::test]
    async fn a_refused_socket_says_you_are_not_authorized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let closed = tmp.path().join("closed");
        std::fs::create_dir_all(&closed).expect("dir");
        let socket = closed.join("meshnode.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).expect("lock");

        let err = connect(&socket)
            .await
            .expect_err("connect should be refused");
        let text = err.to_string();
        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700));

        // Running as root defeats the whole setup; skip rather than lie.
        if !text.contains("not authorized") {
            assert!(
                !text.contains("Permission denied"),
                "unexpected failure shape: {text}"
            );
            return;
        }
        assert!(text.contains("meshnode enrol"), "no way forward: {text}");
        assert!(
            !text.contains("is `meshnode up` running"),
            "sent to the wrong problem entirely: {text}"
        );
    }

    #[test]
    fn finds_the_node_director_embeds() {
        // The case this exists for: a workstation with no daemon of its
        // own, where the only node on the machine is the one Director
        // keeps in its own home and serves the CLI socket from.
        let tmp = tempfile::tempdir().expect("tempdir");
        let default_home = tmp.path().join(".meshware");
        std::fs::create_dir_all(&default_home).expect("home");
        let director = default_home.join("director");
        let _listener = home_with(&director, true);

        assert_eq!(find_node_home(&default_home), Ok(director));
    }

    #[test]
    fn a_socket_nobody_is_listening_on_is_not_a_node() {
        // A unix socket outlives the process that bound it, so the file
        // being there proves nothing. This is the whole reason `knock`
        // connects instead of calling `Path::exists`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let default_home = tmp.path().join(".meshware");
        std::fs::create_dir_all(&default_home).expect("home");
        let director = default_home.join("director");
        home_with(&director, false);

        assert!(
            !matches!(find_node_home(&default_home), Ok(ref home) if *home == director),
            "a stale socket must not pass for a running node"
        );
    }

    #[test]
    fn the_home_that_was_asked_about_wins_over_a_nested_one() {
        // Discovery is a fallback, never a preference: a daemon in the
        // home the caller named is the answer even when Director is up.
        let tmp = tempfile::tempdir().expect("tempdir");
        let default_home = tmp.path().join(".meshware");
        std::fs::create_dir_all(&default_home).expect("home");
        let _default = home_with(&default_home, true);
        let _nested = home_with(&default_home.join("director"), true);

        assert_eq!(find_node_home(&default_home), Ok(default_home));
    }
}
