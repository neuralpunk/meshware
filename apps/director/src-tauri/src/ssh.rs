//! Director's own SSH client.
//!
//! `meshnode adopt` shells out to `ssh(1)` on purpose: a terminal user has
//! a `~/.ssh/config`, an agent, a bastion, a hardware key, and the CLI
//! inherits all of it. A desktop app has a different person in front of
//! it — one who was handed a password and a hostname and wants the
//! machine in the mesh — and on Windows there may be no `ssh` at all. So
//! Director carries a client (`russh`, pure Rust, the same `ring` the
//! transport already links) and takes a password, a key file, or the
//! running agent, from a form.
//!
//! ## Host keys
//!
//! The pairing-code argument (`mesh_core::deploy`) rests on the SSH
//! channel being authentic, so the host key is not waved through. The
//! verdict comes from two files: Director's own `known_hosts` under its
//! data directory, and the user's `~/.ssh/known_hosts`, read but never
//! written. A key that *matches* is known; a key for a host with no entry
//! is learned into Director's file and reported to the screen with its
//! fingerprint, so a person who wants to check it against the console can;
//! a key that **differs** from a recorded one is refused outright, with
//! the fingerprint and the file, exactly as OpenSSH would refuse it.
//!
//! ## Secrets
//!
//! A password lives in the [`Auth`] value for the length of one deploy
//! and is zeroed when that value drops. Nothing here writes one to disk,
//! logs one, or keeps one past the session it opened.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use mesh_core::deploy::Target;

/// TCP connect + key exchange + authentication, all in.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A keepalive every so often, so a NAT between here and a rack does not
/// drop the idle pairing channel while a person reads a code.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long a person has to accept a machine's key before the connection
/// gives up (S-M9). Generous — reading a fingerprint off a console takes a
/// minute — and bounded, because a deploy of forty machines cannot park on
/// one of them for ever.
pub const HOST_KEY_DECISION: Duration = Duration::from_secs(180);

/// How to prove who we are.
#[derive(Clone)]
pub enum Auth {
    Password(Zeroizing<String>),
    Key {
        path: PathBuf,
        passphrase: Option<Zeroizing<String>>,
    },
    /// Whatever the running `ssh-agent` holds.
    Agent,
}

/// The method and the key path, never a password or a passphrase
/// (SEC-23).
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Password(_) => f.write_str("Password(<redacted>)"),
            Auth::Key { path, passphrase } => f
                .debug_struct("Key")
                .field("path", path)
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
            Auth::Agent => f.write_str("Agent"),
        }
    }
}

impl Auth {
    pub fn label(&self) -> &'static str {
        match self {
            Auth::Password(_) => "password",
            Auth::Key { .. } => "key file",
            Auth::Agent => "ssh-agent",
        }
    }
}

/// Where the host key stood when the session opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKeyStatus {
    /// Matched a recorded key.
    Known,
    /// First time seen; recorded now.
    New,
    /// Differs from the recorded key — refused.
    Changed,
}

#[derive(Debug, Clone)]
pub struct HostKey {
    pub algorithm: String,
    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
    pub status: HostKeyStatus,
}

/// What went wrong, in the words a screen can show.
#[derive(Debug, Clone)]
pub enum SshError {
    /// Could not reach the port at all.
    Connect(String),
    /// The host key differs from the one on record.
    HostKeyChanged { fingerprint: String, file: String },
    /// The server refused every credential offered.
    Auth(String),
    /// The session broke, or a step timed out.
    Session(String),
    /// A machine Director had never seen before, and the person did not
    /// accept its key (S-M9).
    HostKeyDeclined { fingerprint: String },
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::Connect(m) | SshError::Auth(m) | SshError::Session(m) => f.write_str(m),
            SshError::HostKeyDeclined { fingerprint } => write!(
                f,
                "the host key ({fingerprint}) was not accepted, so nothing was sent — no \
                 password, no command. Check the fingerprint on the machine itself \
                 (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`) and try again."
            ),
            SshError::HostKeyChanged { fingerprint, file } => write!(
                f,
                "the host key has CHANGED ({fingerprint}) and does not match the one recorded \
                 in {file}. Nothing was sent. If the machine was reinstalled, remove its line \
                 from that file; if it was not, somebody may be answering in its place."
            ),
        }
    }
}

impl std::error::Error for SshError {}

impl From<SshError> for String {
    fn from(e: SshError) -> String {
        e.to_string()
    }
}

// ---------------------------------------------------------------------------
// Host-key policy
// ---------------------------------------------------------------------------

/// "This machine is new — do you accept its key?" (S-M9)
///
/// Asked from inside `check_server_key`, which is **before** anything is
/// authenticated: the point of the whole thing is that the password has not
/// been sent yet. `None` means nobody is there to ask and the old
/// trust-on-first-use behaviour stands, which is what the tests and any
/// non-interactive caller get.
pub type HostKeyApproval = Arc<
    dyn Fn(HostKey) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

struct Handler {
    host: String,
    port: u16,
    /// Director's own store, written on first sight.
    store: PathBuf,
    /// The user's file, consulted but never written.
    user_file: Option<PathBuf>,
    seen: Arc<Mutex<Option<HostKey>>>,
    /// Asked before a *new* key is accepted, and before the password goes.
    approve: Option<HostKeyApproval>,
    /// Set when the person said no, so `connect` can tell that apart from a
    /// key that changed.
    declined: Arc<std::sync::atomic::AtomicBool>,
    /// True while a person is being asked. `connect`'s deadline is about
    /// the *network*, and a person reading a fingerprint off a console is
    /// not the network being slow.
    prompting: Arc<std::sync::atomic::AtomicBool>,
}

impl Handler {
    fn verdict(&self, key: &keys::PublicKey) -> HostKey {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let algorithm = key.algorithm().to_string();
        let files = std::iter::once(self.store.as_path()).chain(self.user_file.as_deref());
        let mut status = HostKeyStatus::New;
        for file in files {
            match keys::check_known_hosts_path(&self.host, self.port, key, file) {
                Ok(true) => {
                    status = HostKeyStatus::Known;
                    break;
                }
                Ok(false) => {}
                Err(keys::Error::KeyChanged { .. }) => {
                    status = HostKeyStatus::Changed;
                    break;
                }
                // An unreadable file is a file with no answer, not a
                // refusal — the user's may not exist, or may be a format
                // the parser skips.
                Err(_) => {}
            }
        }
        HostKey {
            algorithm,
            fingerprint,
            status,
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key: keys::PublicKey = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                keys::PublicKey::from(cert.public_key().clone())
            }
        };
        let verdict = self.verdict(&key);
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(verdict.clone());
        if verdict.status == HostKeyStatus::Changed {
            return Ok(false);
        }
        if verdict.status == HostKeyStatus::New {
            // **The person is asked here, before `authenticate` runs**
            // (S-M9). This used to accept whatever key was offered, record
            // it, and go straight on to send the password — with the
            // fingerprint reported to the screen afterwards, which is too
            // late to be a check. Every subsequent connection was properly
            // verified; it was only the first one that was unprotected, and
            // the first one is the one carrying a freshly typed password.
            //
            // OpenSSH makes the same trade by default — and it asks.
            if let Some(approve) = &self.approve {
                self.prompting
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let accepted = approve(verdict).await;
                self.prompting
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                if !accepted {
                    self.declined
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    return Ok(false);
                }
            }
            // Recorded — the next connection to this host is checked
            // against *this* key, which is what makes a later substitution
            // visible.
            if let Err(e) =
                keys::known_hosts::learn_known_hosts_path(&self.host, self.port, &key, &self.store)
            {
                tracing::warn!(
                    host = %self.host,
                    "could not record the host key in {}: {e}",
                    self.store.display()
                );
            }
        }
        Ok(true)
    }
}

/// Director's own `known_hosts`: beside its other data, never the user's.
pub fn known_hosts_store() -> PathBuf {
    crate::engine::app_dir().join("known_hosts")
}

fn user_known_hosts() -> Option<PathBuf> {
    let p = dirs::home_dir()?.join(".ssh").join("known_hosts");
    p.exists().then_some(p)
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// One authenticated connection to one machine.
pub struct Session {
    handle: Handle<Handler>,
    pub host_key: HostKey,
    pub target: Target,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("target", &self.target)
            .field("host_key", &self.host_key)
            .finish_non_exhaustive()
    }
}

/// What a finished remote command left behind.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub status: u32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// The remote's own words, for an error: the `mw.error=` line when
    /// the script wrote one, else stderr's last lines, else stdout's.
    pub fn complaint(&self) -> String {
        if let Some(e) = mesh_core::deploy::kv_value(&self.stdout, "error") {
            return e.to_string();
        }
        let tail = |s: &str| -> String {
            let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            lines
                .iter()
                .rev()
                .take(3)
                .rev()
                .copied()
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let err = tail(&self.stderr);
        if !err.is_empty() {
            return err;
        }
        let out = tail(&self.stdout);
        if out.is_empty() {
            format!("exit status {}", self.status)
        } else {
            out
        }
    }
}

/// Drive the SSH handshake under [`CONNECT_TIMEOUT`], pausing the clock for
/// as long as a person is being asked about a host key (S-M9).
///
/// `Err(())` is the timeout. The extension is bounded by the approval's own
/// deadline, so this cannot wait for ever whatever the handler does.
async fn connect_within<F, T>(
    connect: F,
    prompting: &std::sync::atomic::AtomicBool,
) -> std::result::Result<T, ()>
where
    F: std::future::Future<Output = T>,
{
    let mut connect = std::pin::pin!(connect);
    let mut sleep = std::pin::pin!(tokio::time::sleep(CONNECT_TIMEOUT));
    loop {
        tokio::select! {
            done = &mut connect => return Ok(done),
            () = &mut sleep => {
                if prompting.load(std::sync::atomic::Ordering::Relaxed) {
                    sleep.as_mut().reset(
                        tokio::time::Instant::now() + HOST_KEY_DECISION + CONNECT_TIMEOUT,
                    );
                } else {
                    return Err(());
                }
            }
        }
    }
}

impl Session {
    /// Connect and authenticate, or say precisely why not.
    pub async fn connect(
        target: &Target,
        user: &str,
        auth: &Auth,
        approve: Option<HostKeyApproval>,
    ) -> Result<Session, SshError> {
        let seen = Arc::new(Mutex::new(None));
        let declined = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prompting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = Handler {
            host: target.host.clone(),
            port: target.port,
            store: known_hosts_store(),
            user_file: user_known_hosts(),
            seen: seen.clone(),
            approve,
            declined: declined.clone(),
            prompting: prompting.clone(),
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(KEEPALIVE),
            keepalive_max: 4,
            ..Default::default()
        });
        let where_ = target.label();
        let connect = client::connect(config, (target.host.as_str(), target.port), handler);
        // `CONNECT_TIMEOUT` is a statement about the *network* — "this
        // machine did not answer" — and a person reading a fingerprint off a
        // console is not the network being slow (S-M9). So the deadline is
        // pushed out only while somebody is actually being asked, and a host
        // that is simply unreachable still fails in twenty seconds rather
        // than three minutes.
        let mut handle = match connect_within(connect, &prompting).await {
            Ok(Ok(h)) => h,
            Ok(Err(russh::Error::UnknownKey)) => {
                let key = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
                let fingerprint = key.map(|k| k.fingerprint).unwrap_or_default();
                if declined.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(SshError::HostKeyDeclined { fingerprint });
                }
                return Err(SshError::HostKeyChanged {
                    fingerprint,
                    file: known_hosts_store().display().to_string(),
                });
            }
            Ok(Err(russh::Error::IO(e))) => {
                return Err(SshError::Connect(connect_hint(&where_, &e)));
            }
            Ok(Err(e)) => {
                return Err(SshError::Connect(format!(
                    "could not open an SSH session to {where_}: {e}"
                )));
            }
            Err(_) => {
                return Err(SshError::Connect(format!(
                    "{where_} did not answer within {}s — is SSH running there, and is the \
                     port right?",
                    CONNECT_TIMEOUT.as_secs()
                )));
            }
        };
        let host_key = seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| SshError::Connect("the server presented no host key".into()))?;

        match tokio::time::timeout(CONNECT_TIMEOUT, authenticate(&mut handle, user, auth)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
                return Err(e);
            }
            Err(_) => {
                return Err(SshError::Auth(format!(
                    "{where_} did not finish authenticating within {}s",
                    CONNECT_TIMEOUT.as_secs()
                )));
            }
        }
        Ok(Session {
            handle,
            host_key,
            target: target.clone(),
        })
    }

    /// Run one command to completion, feeding it `stdin` if given, and
    /// collect everything it said.
    ///
    /// The write runs alongside the read: a command that talks back while
    /// still swallowing a 20 MB binary must not find this side blocked on
    /// the write with its own window full.
    pub async fn run(
        &self,
        command: &str,
        // `Arc<[u8]>`, not `Vec<u8>` (**M-22**). The one large thing ever
        // fed to a remote command is the 18 MB meshnode binary, and a
        // deploy runs up to 32 hosts at once — so an owned copy per host
        // meant ~580 MB of transient heap on a laptop that is also running
        // its own node, for bytes every host is reading identically.
        stdin: Option<std::sync::Arc<[u8]>>,
        within: Duration,
    ) -> Result<Output, SshError> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Session(format!("opening a channel: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::Session(format!("starting the command: {e}")))?;
        let mut writer = channel.make_writer();
        let feed = tokio::spawn(async move {
            if let Some(bytes) = stdin {
                writer.write_all(&bytes).await?;
            }
            writer.shutdown().await
        });
        let mut out = Output::default();
        let mut status = None;
        let read = async {
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        out.stdout.push_str(&String::from_utf8_lossy(&data))
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        out.stderr.push_str(&String::from_utf8_lossy(&data))
                    }
                    ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                    ChannelMsg::ExitSignal { signal_name, .. } => {
                        status = Some(128);
                        out.stderr
                            .push_str(&format!("\nkilled by signal {signal_name:?}"));
                    }
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
        };
        if tokio::time::timeout(within, read).await.is_err() {
            feed.abort();
            return Err(SshError::Session(format!(
                "the command did not finish within {}s",
                within.as_secs()
            )));
        }
        // A write error means the remote closed its end before reading
        // everything: what it printed says why, and that is the error
        // worth surfacing, so the feed's own result is not.
        let _ = feed.await;
        out.status = status.unwrap_or(255);
        Ok(out)
    }

    /// Start a long-lived command and keep both ends of it.
    pub async fn spawn(&self, command: &str) -> Result<Proc, SshError> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Session(format!("opening a channel: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::Session(format!("starting the command: {e}")))?;
        Ok(Proc {
            channel,
            pending: Vec::new(),
            stderr: String::new(),
            exit: None,
            eof: false,
        })
    }

    pub async fn close(self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
    }
}

/// A sentence for the commonest connection failures, instead of an errno.
fn connect_hint(where_: &str, e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => format!(
            "{where_} refused the connection — is SSH running there, and is the port right?"
        ),
        TimedOut => format!("{where_} did not answer — wrong address, or a firewall in between?"),
        NotFound => format!("{where_}: that name does not resolve"),
        _ => {
            let text = e.to_string();
            if text.contains("failed to lookup") || text.contains("nodename nor servname") {
                format!("{where_}: that name does not resolve")
            } else if text.contains("unreachable") {
                format!("{where_} is unreachable from here (no route)")
            } else {
                format!("{where_}: {text}")
            }
        }
    }
}

async fn authenticate(
    handle: &mut Handle<Handler>,
    user: &str,
    auth: &Auth,
) -> Result<(), SshError> {
    match auth {
        Auth::Password(password) => {
            let res = handle
                .authenticate_password(user, password.as_str())
                .await
                .map_err(|e| SshError::Auth(format!("password authentication: {e}")))?;
            if res.success() {
                return Ok(());
            }
            // Many servers only offer `keyboard-interactive` (PAM does
            // this by default): the same password, asked a different way.
            if keyboard_interactive(handle, user, password).await? {
                return Ok(());
            }
            Err(SshError::Auth(format!(
                "{user} was refused with that password — check the user name and the \
                 password, and that password logins are allowed there \
                 (`PasswordAuthentication yes` in sshd_config)"
            )))
        }
        Auth::Key { path, passphrase } => {
            let key = keys::load_secret_key(path, passphrase.as_ref().map(|p| p.as_str()))
                .map_err(|e| {
                    let text = e.to_string();
                    SshError::Auth(
                        if text.to_ascii_lowercase().contains("passphrase")
                            || text.to_ascii_lowercase().contains("decrypt")
                            || text.to_ascii_lowercase().contains("password")
                        {
                            format!(
                                "{} is protected by a passphrase, and the one given did not \
                             unlock it",
                                path.display()
                            )
                        } else {
                            format!("could not read the key {}: {text}", path.display())
                        },
                    )
                })?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?
                .flatten();
            let res = handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| SshError::Auth(format!("key authentication: {e}")))?;
            if res.success() {
                Ok(())
            } else {
                Err(SshError::Auth(format!(
                    "{user} was refused with the key {} — is its public half in that \
                     account's ~/.ssh/authorized_keys?",
                    path.display()
                )))
            }
        }
        Auth::Agent => agent_auth(handle, user).await,
    }
}

async fn keyboard_interactive(
    handle: &mut Handle<Handler>,
    user: &str,
    password: &str,
) -> Result<bool, SshError> {
    let mut step = handle
        .authenticate_keyboard_interactive_start(user, None)
        .await
        .map_err(|e| SshError::Auth(format!("keyboard-interactive: {e}")))?;
    // A handful of rounds is every real PAM stack; a server that keeps
    // asking is a server that is not going to say yes.
    //
    // **The password is offered once** (S-L7). Every hidden prompt used to
    // get it, on every round — so a PAM stack asking for a TOTP code with
    // echo off received the password instead (harmless against an honest
    // server, useless against the stack that wanted a code), and a hostile
    // server could collect it once per prompt rather than once. A password
    // login has exactly one password in it; after that there is nothing
    // left to say.
    let mut offered = false;
    for _ in 0..4 {
        match step {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers: Vec<String> = prompts
                    .iter()
                    .map(|p| {
                        // The first hidden prompt is the password; an echoed
                        // one is something else (a token?) we cannot answer,
                        // and an empty string says so. So is a *second*
                        // hidden one — there is only one password here, and
                        // sending it again answers a question nobody asked.
                        if p.echo || offered {
                            String::new()
                        } else {
                            offered = true;
                            password.to_string()
                        }
                    })
                    .collect();
                step = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|e| SshError::Auth(format!("keyboard-interactive: {e}")))?;
            }
        }
    }
    Ok(false)
}

async fn agent_auth(handle: &mut Handle<Handler>, user: &str) -> Result<(), SshError> {
    use russh::keys::agent::AgentIdentity;
    use russh::keys::agent::client::AgentClient;

    let mut agent = AgentClient::connect_env().await.map_err(|e| {
        SshError::Auth(format!(
            "no ssh-agent reachable ({e}) — start one and add your key, or use a key file or \
             a password instead"
        ))
    })?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| SshError::Auth(format!("asking the agent for keys: {e}")))?;
    if identities.is_empty() {
        return Err(SshError::Auth(
            "the ssh-agent holds no keys — `ssh-add` one, or use a key file or a password".into(),
        ));
    }
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| SshError::Auth(e.to_string()))?
        .flatten();
    let mut tried = 0;
    for id in identities {
        let key = match id {
            AgentIdentity::PublicKey { key, .. } => key,
            // Certificates are a different dance; skip them here.
            _ => continue,
        };
        tried += 1;
        let res = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| SshError::Auth(format!("agent authentication: {e}")))?;
        if res.success() {
            return Ok(());
        }
    }
    Err(SshError::Auth(format!(
        "{user} was refused with every key the agent holds ({tried} tried) — is one of their \
         public halves in that account's ~/.ssh/authorized_keys?"
    )))
}

// ---------------------------------------------------------------------------
// A long-lived remote process
// ---------------------------------------------------------------------------

/// A remote command with its stdin still open: the joiner.
pub struct Proc {
    channel: russh::Channel<client::Msg>,
    /// Bytes of stdout not yet returned as a whole line.
    pending: Vec<u8>,
    stderr: String,
    exit: Option<u32>,
    eof: bool,
}

impl std::fmt::Debug for Proc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proc")
            .field("exit", &self.exit)
            .field("eof", &self.eof)
            .finish_non_exhaustive()
    }
}

impl Proc {
    /// The next line of stdout, or `None` once the process is gone.
    pub async fn next_line(&mut self, within: Duration) -> Result<Option<String>, SshError> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=pos).collect();
                return Ok(Some(String::from_utf8_lossy(&line).trim_end().to_string()));
            }
            if self.eof {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let rest = std::mem::take(&mut self.pending);
                return Ok(Some(String::from_utf8_lossy(&rest).trim_end().to_string()));
            }
            let msg = tokio::time::timeout_at(deadline, self.channel.wait())
                .await
                .map_err(|_| SshError::Session("the remote process went quiet".into()))?;
            match msg {
                Some(ChannelMsg::Data { data }) => self.pending.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    self.stderr.push_str(&String::from_utf8_lossy(&data));
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => self.exit = Some(exit_status),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => self.eof = true,
                Some(_) => {}
            }
        }
    }

    /// Write to the process's stdin.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
        self.channel
            .data(bytes)
            .await
            .map_err(|e| SshError::Session(format!("writing to the remote process: {e}")))
    }

    /// Wait for the process to end, up to `within`. The exit status, if it
    /// reported one.
    ///
    /// The server sends EOF on the data stream *before* the exit status,
    /// so reading lines to their end is not enough: the messages after
    /// EOF are drained until the channel closes.
    pub async fn wait(&mut self, within: Duration) -> Option<u32> {
        let deadline = tokio::time::Instant::now() + within;
        let _ = tokio::time::timeout_at(deadline, async {
            while self.next_line(within).await.ok().flatten().is_some() {}
        })
        .await;
        while self.exit.is_none() {
            let msg = match tokio::time::timeout_at(deadline, self.channel.wait()).await {
                Ok(Some(m)) => m,
                Ok(None) | Err(_) => break,
            };
            match msg {
                ChannelMsg::ExitStatus { exit_status } => self.exit = Some(exit_status),
                ChannelMsg::ExtendedData { data, .. } => {
                    self.stderr.push_str(&String::from_utf8_lossy(&data));
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        self.exit
    }

    /// End the process now: a signal, then the channel.
    pub async fn kill(&mut self) {
        let _ = self.channel.signal(russh::Sig::TERM).await;
        let _ = self.channel.eof().await;
        let _ = self.channel.close().await;
        self.eof = true;
    }

    /// Whatever it wrote to stderr so far.
    pub fn stderr(&self) -> &str {
        self.stderr.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complaint_prefers_the_script_s_own_sentence() {
        let o = Output {
            status: 1,
            stdout: "noise\nmw.error=no useradd here\n".into(),
            stderr: "sh: line 4: useradd: not found\n".into(),
        };
        assert_eq!(o.complaint(), "no useradd here");
        let o = Output {
            status: 2,
            stdout: String::new(),
            stderr: "a\n\nb\nc\nd\n".into(),
        };
        assert_eq!(o.complaint(), "b · c · d");
        let o = Output {
            status: 3,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(o.complaint(), "exit status 3");
    }

    /// The real client against a real server.
    ///
    /// Ignored by default: it needs an sshd to talk to. `scripts/test-ssh-local.sh`
    /// starts a throwaway one on a high port with a fresh key and runs this
    /// with `MESHWARE_TEST_SSH=user@host:port` and `MESHWARE_TEST_SSH_KEY=<path>`.
    #[tokio::test]
    #[ignore = "needs MESHWARE_TEST_SSH / MESHWARE_TEST_SSH_KEY and a reachable sshd"]
    async fn a_session_runs_commands_feeds_stdin_and_streams_lines() {
        let Ok(target) = std::env::var("MESHWARE_TEST_SSH") else {
            return;
        };
        let key = std::env::var("MESHWARE_TEST_SSH_KEY").expect("MESHWARE_TEST_SSH_KEY");
        // Director's data dir may not exist on a test box; the store is
        // created on first learn.
        let target = Target::parse(&target).expect("target");
        let user = target.user.clone().expect("user@ in MESHWARE_TEST_SSH");
        let auth = Auth::Key {
            path: PathBuf::from(key),
            passphrase: None,
        };
        // `None`: nobody is watching a test, so the old trust-on-first-use
        // path is the one under test here.
        let session = Session::connect(&target, &user, &auth, None)
            .await
            .expect("connect + auth");
        assert!(!session.host_key.fingerprint.is_empty());

        // A plain command, with its exit status.
        let out = session
            .run(
                "printf 'mw.os=%s\\n' \"$(uname -s)\"; exit 3",
                None,
                Duration::from_secs(20),
            )
            .await
            .expect("run");
        assert_eq!(out.status, 3);
        assert_eq!(
            mesh_core::deploy::kv_value(&out.stdout, "os").map(|s| s.to_string()),
            Some(
                String::from_utf8_lossy(
                    &std::process::Command::new("uname")
                        .arg("-s")
                        .output()
                        .expect("uname")
                        .stdout
                )
                .trim()
                .to_string()
            )
        );

        // Two megabytes of stdin, hashed on the far side: the write and the
        // read interleave without deadlocking, and every byte arrives.
        let bytes: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
        let want = blake3::hash(&bytes).to_hex().to_string();
        let out = session
            .run(
                "sh -c 'cat > \"$HOME/.mw-ssh-test\"; wc -c < \"$HOME/.mw-ssh-test\"; rm -f \"$HOME/.mw-ssh-test\"'",
                Some(bytes.clone().into()),
                Duration::from_secs(60),
            )
            .await
            .expect("feed");
        assert!(out.ok(), "{}", out.complaint());
        assert_eq!(out.stdout.trim(), bytes.len().to_string());
        let _ = want;

        // A long-lived process: lines come back one at a time, stdin still
        // open, and an answer on stdin reaches it.
        let mut proc = session
            .spawn("sh -c 'echo first; read -r answer; echo \"got $answer\"; echo done'")
            .await
            .expect("spawn");
        assert_eq!(
            proc.next_line(Duration::from_secs(10))
                .await
                .expect("line")
                .as_deref(),
            Some("first")
        );
        proc.write(b"yes\n").await.expect("write");
        assert_eq!(
            proc.next_line(Duration::from_secs(10))
                .await
                .expect("line")
                .as_deref(),
            Some("got yes")
        );
        assert_eq!(
            proc.next_line(Duration::from_secs(10))
                .await
                .expect("line")
                .as_deref(),
            Some("done")
        );
        assert_eq!(proc.wait(Duration::from_secs(10)).await, Some(0));

        // The stage script itself, end to end: the binary is a shell
        // script here, which `--version` answers like a meshnode would.
        let fake = b"#!/bin/sh\necho meshnode 9.9.9\n".to_vec();
        let out = session
            .run(
                &format!(
                    "sh -c {}",
                    mesh_core::deploy::shell_quote(&mesh_core::deploy::InstallPlan::stage_script())
                ),
                Some(fake.into()),
                Duration::from_secs(30),
            )
            .await
            .expect("stage");
        assert!(out.ok(), "{}", out.complaint());
        let staged = mesh_core::deploy::kv_value(&out.stdout, "staged")
            .expect("staged")
            .to_string();
        assert_eq!(
            mesh_core::deploy::kv_value(&out.stdout, "version"),
            Some("meshnode 9.9.9")
        );
        let _ = session
            .run(
                &format!("rm -f {}", mesh_core::deploy::shell_quote(&staged)),
                None,
                Duration::from_secs(10),
            )
            .await;
        session.close().await;
    }

    #[test]
    fn connection_failures_become_sentences() {
        let e = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x");
        assert!(connect_hint("ops@rack-1", &e).contains("is SSH running"));
        let e = std::io::Error::other("failed to lookup address information");
        assert!(connect_hint("rack-9", &e).contains("does not resolve"));
    }
}
