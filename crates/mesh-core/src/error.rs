#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity error: {0}")]
    Identity(String),
    /// A sealed user key with no unlock available. Distinct from
    /// [`CoreError::Identity`] because callers act on it: the CLI prompts,
    /// and `load_cli_user` must stop rather than fall through to minting a
    /// second identity.
    #[error(
        "the identity at {0} is locked.\n\n\
         Unlock it for this login session:\n    meshnode identity unlock\n\n\
         Or, where there is no session to keep it in — CI, cron, a container — \
         set MESHWARE_PASSPHRASE for the command."
    )]
    IdentityLocked(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("genesis error: {0}")]
    Genesis(String),
    #[error("tls error: {0}")]
    Tls(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// The peer was not reached, so nothing was delivered: a dial that
    /// failed, or a stream that could not be opened or written (ENG-6).
    /// Unlike [`CoreError::Transport`] after a request went out, this one
    /// makes trying another candidate safe.
    #[error("unreachable: {0}")]
    Unreachable(String),
    #[error("handshake refused: {0}")]
    HandshakeRefused(String),
    /// The peer negotiated a protocol minor below what this request needs,
    /// so it was never sent (ENG-24).
    ///
    /// Its own variant rather than a [`CoreError::Transport`] because the
    /// two mean opposite things to a caller: nothing was delivered, so
    /// another candidate is safe — and asking *this* peer again will give
    /// the same answer until somebody upgrades it. That is a sentence an
    /// operator can act on, where the symptom it replaces was a dropped
    /// stream and a timeout.
    #[error("peer too old: {0}")]
    PeerTooOld(String),
    #[error("protocol error: {0}")]
    Proto(#[from] mesh_proto::ProtoError),
    #[error("ipc error: {0}")]
    Ipc(String),
    #[error("store error: {0}")]
    Store(String),
    /// Data on disk written by a newer release than this binary (ENG-1).
    /// Its own variant because the daemon exits with its own status for
    /// it: a restart cannot fix it, so systemd must not keep trying.
    #[error("{0}")]
    TooNew(String),
    /// This home left a mesh when its founder reset it, and has joined
    /// none since (`crate::reset`). Its own variant because the daemon
    /// exits with a status its unit does not restart: a restart would only
    /// ask the same question.
    #[error("{0}")]
    AwaitingAdoption(String),
    /// A loop the node cannot work without has ended (ENG-2). The daemon
    /// stops with this rather than carrying on as a node nobody can use.
    #[error("the node stopped working: {0}")]
    Failed(String),
    #[error("state error: {0}")]
    State(String),
    #[error("membership error: {0}")]
    Membership(String),
    #[error("job error: {0}")]
    Job(String),
    #[error("secret error: {0}")]
    Secret(String),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("auth error: {0}")]
    Auth(String),
    /// Definitive backpressure refusal: slots and queue full (MASTERPLAN
    /// §10.2). The only error that triggers spill-over to another candidate.
    #[error("node is saturated ({running} running, {queued} queued)")]
    Busy { running: u32, queued: u32 },
    /// One entity is authoring ops faster than this node will carry (M31
    /// A3). Its own variant rather than a `State` string because a caller
    /// has to be able to tell *"you are looping"* from *"something broke"*:
    /// the first is the caller's bug and the message names it, the second
    /// is ours.
    #[error("{message}")]
    Backpressure { entity: String, message: String },
}

pub type Result<T> = std::result::Result<T, CoreError>;
