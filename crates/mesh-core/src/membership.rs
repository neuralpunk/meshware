//! Membership: invite tokens, the join flow, revocation, and target
//! selectors (MASTERPLAN §4.2, §5).
//!
//! Zero-config joining: `meshnode invite create` on any member mints a
//! single-string token; `meshnode join <token>` on a fresh machine does the
//! rest. No certificates, no cluster IDs, no UI uploads.

use std::sync::Arc;

use tracing::{info, warn};

use crate::config::NodeConfig;
use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::identity::{Identity, Keypair, verify_signature};
use crate::rbac;
use crate::state::{MeshState, StateEngine};
use crate::transport::{LocalInfo, Transport};
use mesh_proto::{
    BOOTSTRAP_MINOR, InviteId, InvitePublic, InviteToken, JoinAccept, JoinAcceptV2, JoinRequest,
    Liveness, MemberInfo, NodeId, NodeRevocation, OPLOG_MINOR, OpBody, OpEnvelope, Request,
    Requirements, Response, SnapshotChunk, SystemSnapshot, VersionVector, invite_signing_bytes,
};

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

pub struct InviteParams {
    pub ttl_secs: u64,
    pub uses: u32,
    pub tags: Vec<String>,
    /// Reachable endpoints to embed in the token.
    pub bootstrap: Vec<String>,
}

impl std::fmt::Debug for InviteParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InviteParams(ttl={}s uses={})", self.ttl_secs, self.uses)
    }
}

/// Build and sign an invite token + its public record with a user key,
/// without touching an engine. This is the client-side path: only the CLI /
/// Director holds the user's private key (MASTERPLAN §7.1).
pub fn build_invite(
    mesh_id: mesh_proto::MeshId,
    user_key: &Keypair,
    params: &InviteParams,
    created_by_node: NodeId,
) -> Result<(InviteToken, InvitePublic)> {
    if params.bootstrap.is_empty() {
        return Err(CoreError::Membership(
            "invite needs at least one reachable bootstrap address".into(),
        ));
    }
    let mut token = InviteToken {
        mesh_id,
        invite_id: InviteId::generate(),
        expires_at_micros: mesh_proto::now_micros() + params.ttl_secs * 1_000_000,
        uses: params.uses.max(1),
        tags: params.tags.clone(),
        bootstrap: params.bootstrap.clone(),
        created_by_user: user_key.user_id(),
        signature: Vec::new(),
    };
    token.signature = user_key.sign(&invite_signing_bytes(&token)).to_vec();

    let mut public = InvitePublic {
        invite_id: token.invite_id,
        expires_at_micros: token.expires_at_micros,
        uses: token.uses,
        tags: token.tags.clone(),
        created_by: created_by_node,
        created_by_user: user_key.user_id(),
        signature: Vec::new(),
    };
    public.signature = user_key.sign(&public.signing_bytes()).to_vec();
    Ok((token, public))
}

/// Mint an invite token on a node that holds the admin user key (bootstrap
/// founder path and tests). Production clients use [`build_invite`] plus the
/// `InvitePublish` IPC request.
pub fn create_invite(
    engine: &StateEngine,
    user_key: &Keypair,
    params: InviteParams,
) -> Result<InviteToken> {
    let genesis = engine.genesis();
    let founder = genesis.body.founder_user;
    let user = user_key.user_id();
    let is_admin = engine.read(|s| {
        rbac::has_role(
            s,
            &founder,
            &user,
            rbac::ADMIN_ONLY,
            mesh_proto::now_micros(),
        )
    });
    if !is_admin {
        return Err(CoreError::Denied("only admins can mint invites".into()));
    }
    let (token, public) = build_invite(genesis.body.mesh_id, user_key, &params, engine.node_id())?;
    engine.author_op(OpBody::InviteCreate { invite: public })?;
    Ok(token)
}

/// Validate a presented token against genesis + current state. The signer
/// must be the founder or a currently-active admin, and the invite must be
/// one this mesh has a record of.
///
/// **An invite nobody published is refused** (SEC-17). This used to admit
/// any admin-signed token it had not heard of, enforcing the use budget
/// only "if we know the invite" — so a single-use token presented to a node
/// the create had not reached yet admitted as many joiners as it was shown
/// to, and a revoke that had not reached it either did nothing. A node
/// that answers a join first asks its peers ([`invite_not_yet_known`]),
/// which is what makes the refusal safe to give.
pub fn validate_token(engine: &StateEngine, token: &InviteToken) -> Result<()> {
    validate_token_signer(engine, token)?;
    match engine.read(|s| s.invite_uses_left(&token.invite_id)) {
        None => Err(CoreError::Membership(
            "this node has no record of that invite — it may not have reached \
             this node yet (try again in a moment, or join through the node \
             that created it)"
                .into(),
        )),
        Some(0) => Err(CoreError::Membership(
            "invite has been revoked or has no uses left".into(),
        )),
        Some(_) => Ok(()),
    }
}

/// Whether `token` is sound — right mesh, signed by an active admin, not
/// expired — but names an invite this node has no record of: the one
/// refusal that syncing with a peer can cure (SEC-17).
///
/// Checked before any sync is spent on it, so a stranger with a made-up
/// token costs one signature check and nothing else.
pub fn invite_not_yet_known(engine: &StateEngine, token: &InviteToken) -> bool {
    validate_token_signer(engine, token).is_ok()
        && engine.read(|s| !s.invites.contains_key(&token.invite_id))
}

fn validate_token_signer(engine: &StateEngine, token: &InviteToken) -> Result<()> {
    let genesis = engine.genesis();
    if token.mesh_id != genesis.body.mesh_id {
        return Err(CoreError::Membership(
            "invite is for a different mesh".into(),
        ));
    }
    let sig: [u8; 64] = token
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| CoreError::Membership("invite signature malformed".into()))?;
    if !verify_signature(&token.created_by_user.0, &invite_signing_bytes(token), &sig) {
        return Err(CoreError::Membership("invite signature invalid".into()));
    }
    let founder = genesis.body.founder_user;
    let signer_is_admin = engine.read(|s| {
        rbac::has_role(
            s,
            &founder,
            &token.created_by_user,
            rbac::ADMIN_ONLY,
            mesh_proto::now_micros(),
        )
    });
    if !signer_is_admin {
        return Err(CoreError::Membership(
            "invite was not signed by an active admin".into(),
        ));
    }
    if mesh_proto::now_micros() > token.expires_at_micros {
        return Err(CoreError::Membership("invite has expired".into()));
    }
    Ok(())
}

/// Server side of a join: validate, admit, and hand back genesis + ops in
/// one frame.
///
/// The snapshot is length-delimited ([`JoinAcceptV2`]); [`downgrade_join`]
/// cuts it down for a pre-1.7 joiner. Serving it whole is only correct
/// while it *fits* — see [`oversized_snapshot_refusal`], which is why this
/// is no longer the path a current joiner takes ([`handle_join_chunked`]
/// is).
pub fn handle_join(
    engine: &StateEngine,
    joiner: NodeId,
    observed_ip: std::net::IpAddr,
    req: &JoinRequest,
) -> Result<JoinAcceptV2> {
    admit(engine, joiner, observed_ip, req)?;
    let genesis_json = encode_genesis(engine)?;
    Ok(JoinAcceptV2 {
        genesis_json,
        ops: engine.all_ops()?,
    })
}

/// Server side of a join for a 1.10 joiner: admit, then hand back the
/// *first* instalment of the snapshot. The joiner asks for the rest with
/// `JoinFetch` (see [`snapshot_chunk`]).
pub fn handle_join_chunked(
    engine: &StateEngine,
    joiner: NodeId,
    observed_ip: std::net::IpAddr,
    req: &JoinRequest,
) -> Result<SnapshotChunk> {
    admit(engine, joiner, observed_ip, req)?;
    snapshot_chunk(engine, &Vec::new(), true)
}

/// One instalment of a bootstrap snapshot: what the holder of `have` is
/// still missing, bounded by the batch rule anti-entropy already uses.
///
/// This is `missing_for` under a different name, and deliberately so. A
/// bootstrap is not a special kind of transfer — it is anti-entropy from an
/// empty version vector — so it gets the same walk, the same per-author
/// gapless ordering, and the same `more` flag. The one addition is genesis,
/// which rides the first chunk because the receiver cannot verify a single
/// op without it.
pub fn snapshot_chunk(
    engine: &StateEngine,
    have: &VersionVector,
    with_genesis: bool,
) -> Result<SnapshotChunk> {
    let (ops, more) = engine.missing_for(have)?;
    Ok(SnapshotChunk {
        genesis_json: if with_genesis {
            encode_genesis(engine)?
        } else {
            Vec::new()
        },
        ops,
        more,
    })
}

/// What a bootstrap receiver can honestly claim to hold, across instalments.
///
/// This used to be one line — take the max seq per author — and that was
/// right while the log was append-only and every chunk was per-author
/// gapless in sequence order. Compaction breaks both halves of that
/// (C0 §4). A chunk may now contain a hole its author vouched for, and it
/// may carry the checkpoint that vouches for it *out of sequence order*,
/// because the checkpoint sits at the top of the log and the point of
/// sending it early is to explain what is missing below.
///
/// Taking the max under those conditions is not merely imprecise, it loses
/// data: the receiver would claim the checkpoint's high sequence number,
/// the sender would conclude it is caught up on that author, and every real
/// op between the hole and the checkpoint would never be sent.
///
/// So the claim is *contiguity*: the largest `s` where every seq up to `s`
/// has either arrived or been vouched for as gone.
#[derive(Debug, Default)]
pub struct SnapshotProgress {
    mark: std::collections::BTreeMap<NodeId, u64>,
    pruned: crate::compact::PrunedMap,
    /// Seqs that arrived ahead of their predecessor, held until the gap
    /// closes. Bounded by how far out of order one transfer runs, which
    /// for a chunked snapshot is one checkpoint per compacted author.
    ahead: std::collections::BTreeMap<NodeId, std::collections::BTreeSet<u64>>,
}

impl SnapshotProgress {
    /// Fold in one instalment.
    pub fn observe(&mut self, ops: &[OpEnvelope]) {
        // Checkpoints first: a hole is only steppable once its author has
        // vouched for it, and the op that vouches may be later in the slice
        // than the ops around the hole.
        for op in ops {
            if let Some(OpBody::Checkpoint { checkpoint }) = op.decode_body()
                && checkpoint.is_well_formed()
            {
                self.pruned
                    .entry(op.author)
                    .or_default()
                    .insert_all(&checkpoint.subsumed);
            }
        }
        for op in ops {
            self.ahead.entry(op.author).or_default().insert(op.seq);
        }
        for (author, held) in &mut self.ahead {
            let mark = self.mark.entry(*author).or_insert(0);
            let pruned = self.pruned.get(author);
            loop {
                let next = *mark + 1;
                if held.remove(&next) || pruned.is_some_and(|p| p.contains(next)) {
                    *mark = next;
                } else {
                    break;
                }
            }
        }
    }

    /// The version vector to ask the sender with.
    pub fn vv(&self) -> VersionVector {
        self.mark
            .iter()
            .filter(|(_, seq)| **seq > 0)
            .map(|(a, seq)| (*a, *seq))
            .collect()
    }
}

/// Why a pre-1.10 joiner cannot be served, when it cannot.
///
/// The single-frame snapshot is capped at `MAX_FRAME_LEN` and an op-log
/// only grows, so every mesh eventually passes the point where the old
/// shape cannot express its history. Encoding it and letting the codec
/// refuse gives the operator `frame too large: 4493850 bytes` from three
/// layers down; this gives them the sentence that names the fix.
pub fn oversized_snapshot_refusal<T: serde::Serialize>(
    accept: &T,
    peer_minor: u16,
) -> Option<String> {
    // Measure what will actually be written, not an equivalent of it: the
    // V1 downgrade withholds ops the joiner cannot decode, so a snapshot
    // that is too large as envelopes can be servable as `Vec<Op>`.
    let len = postcard::to_stdvec(accept).map(|b| b.len()).unwrap_or(0);
    if len <= mesh_proto::MAX_FRAME_LEN as usize {
        return None;
    }
    Some(format!(
        "this mesh's history is {len} bytes, more than the {} a single join frame can carry, and this joiner speaks protocol 1.{peer_minor} — which has no way to receive a snapshot in instalments. Upgrade the joining node to 1.{BOOTSTRAP_MINOR} or later.",
        mesh_proto::MAX_FRAME_LEN
    ))
}

fn encode_genesis(engine: &StateEngine) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(engine.genesis())
        .map_err(|e| CoreError::Membership(format!("encode genesis: {e}")))
}

/// Validate the invite and author the membership. Shared by every join
/// shape, because which snapshot the joiner can read has no bearing on
/// whether it is allowed in.
fn admit(
    engine: &StateEngine,
    joiner: NodeId,
    observed_ip: std::net::IpAddr,
    req: &JoinRequest,
) -> Result<()> {
    validate_token(engine, &req.token)?;
    // Before the invite is spent (SEC-9): a name that could repaint a
    // terminal is refused here, where the joiner can be told, rather than
    // replicated into every table the mesh prints.
    if let Some(problem) = mesh_proto::text::name_problem(&req.node_name) {
        return Err(CoreError::Membership(problem));
    }
    if engine.read(|s| s.revoked.contains_key(&joiner)) {
        return Err(CoreError::Membership(
            "this node key has been revoked".into(),
        ));
    }

    // Port 0 means "the joiner will bind an ephemeral port at `up`" — an
    // observed_ip:0 candidate is never dialable, so record nothing and let
    // the joiner's boot-time MemberUpdate announce its real address. A
    // synthetic (relay-routed) observed address is never a real endpoint
    // either (MASTERPLAN M7).
    let mut addrs = if req.listen_port > 0 && !crate::meshsock::is_synthetic_ip(observed_ip) {
        vec![format!("{observed_ip}:{}", req.listen_port)]
    } else {
        Vec::new()
    };
    for a in &req.addrs {
        if !addrs.contains(a) {
            addrs.push(a.clone());
        }
    }
    let mut tags = req.token.tags.clone();
    for t in &req.extra_tags {
        if !tags.contains(t) {
            tags.push(t.clone());
        }
    }

    let member = MemberInfo {
        node_id: joiner,
        name: req.node_name.clone(),
        addrs,
        tags,
        slots: req.slots,
        joined_at_micros: mesh_proto::now_micros(),
    };
    engine.author_op(OpBody::MemberAdd {
        member,
        invite: Some(req.token.invite_id),
    })?;
    engine.author_op(OpBody::InviteConsume {
        invite_id: req.token.invite_id,
        node_id: joiner,
    })?;
    info!(joiner = %joiner.short(), name = %req.node_name, "node joined the mesh");
    Ok(())
}

/// Whether this node can admit a pre-1.7 joiner at all (F0 §3.3), and if
/// not, why.
///
/// The sync downgrade leaves a legacy peer *stably behind*; a legacy
/// **joiner** is a stronger case, because the op that makes it a member is
/// one this node is about to author — and it would land after the cut. Per-
/// author `seq` is gapless, so a snapshot truncated before this node's
/// first future op cannot contain the joiner's own `MemberAdd`, and the
/// joiner would come up believing it is not in the mesh.
///
/// Refuse instead, before authoring anything: an invite use is not spent, no
/// half-member is left in replicated state, and the operator gets a message
/// with two real remedies. Note the cut is **per author** — a member that
/// has authored no future ops can still admit this joiner, which is often
/// the faster fix in the middle of an upgrade.
pub fn legacy_join_refusal(engine: &StateEngine) -> Option<String> {
    let me = engine.node_id();
    let cut = engine.read(|s| s.legacy_cut.get(&me).copied())?;
    Some(format!(
        "this joiner speaks a protocol older than 1.{OPLOG_MINOR} and cannot receive \
         records this mesh already holds (from op {cut} of this member onward), \
         including the membership record admitting it. Upgrade the joining node, \
         or join through a member that has not authored any."
    ))
}

/// Cut a join snapshot down to what a pre-1.7 joiner can decode (F0 §3.3).
///
/// The same rule as the sync downgrade, for the same reason: postcard
/// decodes `Vec<Op>` atomically, so one op the joiner has no variant for
/// would fail the entire accept frame and the join with it. It joins on the
/// prefix it can read and is stably behind on the rest, which beats not
/// joining at all — and it upgrades out of the state by upgrading.
pub fn downgrade_join(accept: &JoinAcceptV2) -> JoinAccept {
    let (ops, cut) = mesh_proto::downgrade_for_v1(&accept.ops);
    if !cut.is_empty() {
        warn!(
            authors = cut.len(),
            withheld = accept.ops.len() - ops.len(),
            "joiner predates the forward-compatible op-log; \
             serving the snapshot it can decode"
        );
    }
    JoinAccept {
        genesis_json: accept.genesis_json.clone(),
        ops,
    }
}

/// What `meshnode join` reports on success.
#[derive(Debug)]
pub struct JoinReport {
    pub mesh_name: String,
    pub accepted_by: NodeId,
    pub members: usize,
}

/// Client side of a join. Runs *without* a daemon: dials a bootstrap node,
/// presents the token, persists genesis + ops, done. `meshnode up` after
/// this participates as a member.
///
/// `via` prepends explicit bootstrap addresses to the token's embedded ones
/// — the escape hatch when the minting node's advertised address is not the
/// one this machine can reach (VPN/Tailscale overlays, NAT'd workstations;
/// the v1 WAN answer until M7's candidate discovery).
pub async fn join_mesh(config: &NodeConfig, token_str: &str, via: &[String]) -> Result<JoinReport> {
    if Genesis::load(&config.data_dir)?.is_some() {
        return Err(CoreError::Membership(
            "this node is already in a mesh (genesis exists). Remove the data dir to start over."
                .into(),
        ));
    }
    let token = InviteToken::decode(token_str)
        .map_err(|_| CoreError::Membership("invite token is malformed".into()))?;
    if mesh_proto::now_micros() > token.expires_at_micros {
        return Err(CoreError::Membership("invite has expired".into()));
    }

    let identity = Identity::load_or_generate(&config.data_dir)?;
    let local = LocalInfo {
        node_id: identity.node.node_id(),
        node_name: config.node_name.clone(),
        mesh_id: Some(token.mesh_id),
        mesh_name: None,
    };
    // Ephemeral endpoint just for the join dial.
    let admission: crate::transport::AdmissionCheck =
        Arc::new(|_, _| Err("joining node accepts no inbound connections".into()));
    let (transport, _events) = Transport::bind(
        "0.0.0.0:0".parse().expect("valid bind"),
        identity.node.clone(),
        local,
        admission,
        config.bind_options(),
    )
    .await?;

    let join_req = JoinRequest {
        token: token.clone(),
        node_name: config.node_name.clone(),
        listen_port: config.listen.port(),
        // Operator-declared public endpoints travel with the join so the
        // acceptor records them immediately (MASTERPLAN M7 `[wan]`).
        addrs: config.wan.advertise.clone(),
        // Capacity travels with the join so the acceptor records real
        // hardware immediately, rather than waiting for the joiner's
        // boot-time re-announcement (M9 §5.1).
        //
        // **No runtimes yet**, and that is correct rather than a gap: the
        // probes have not run on a node that is still joining, and an
        // absent `cap.rt` means *unknown*, which is neutral. The joiner's
        // own re-announce fills it in seconds later. Announcing an empty
        // list here would say "can run nothing" about a machine nobody has
        // asked yet.
        extra_tags: crate::capacity::announced_tags(config, &[]),
        slots: config.limits.slots,
    };

    // `--via` first (the operator knows better), then the token's
    // rendezvous set. The transport races them (staggered) and skips
    // `relay:` entries — a joiner is not a member and cannot use relays.
    let mut bootstrap: Vec<String> = via.to_vec();
    for a in &token.bootstrap {
        if !bootstrap.contains(a) {
            bootstrap.push(a.clone());
        }
    }
    let result = async {
        let (conn, ack) = transport.race_join(&bootstrap).await?;
        join_via(&transport, conn, ack, &join_req, &token, config, &identity).await
    }
    .await;
    transport.close();
    result
}

async fn join_via(
    transport: &Transport,
    conn: quinn::Connection,
    ack: mesh_proto::ConnHelloAck,
    join_req: &JoinRequest,
    token: &InviteToken,
    config: &NodeConfig,
    identity: &Identity,
) -> Result<JoinReport> {
    // The handshake already told us what the acceptor speaks, so the
    // joiner never has to guess which snapshot shape to ask for.
    let minor = mesh_proto::negotiate(
        (mesh_proto::PROTOCOL_MAJOR, transport.advertised_minor()),
        (ack.proto_major, ack.proto_minor),
    )
    .map_err(CoreError::Membership)?;
    let request = if minor >= BOOTSTRAP_MINOR {
        Request::JoinV3(Box::new(join_req.clone()))
    } else if minor >= OPLOG_MINOR {
        Request::JoinV2(Box::new(join_req.clone()))
    } else {
        Request::Join(join_req.clone())
    };
    let resp = transport.request_one(&conn, &request).await?;
    let accept = match resp {
        // A 1.10 acceptor: the first instalment, then ask for the rest.
        Response::Snapshot(chunk) => pull_snapshot(transport, &conn, *chunk).await?,
        Response::JoinAcceptV2(a) => *a,
        // A pre-1.7 acceptor. Its snapshot is decoded ops by definition —
        // it has no other kind — so wrapping them loses nothing.
        Response::JoinAccept(a) => JoinAcceptV2 {
            genesis_json: a.genesis_json,
            ops: a.ops.iter().map(OpEnvelope::from_op).collect(),
        },
        Response::Error { message } => return Err(CoreError::Membership(message)),
        other => {
            return Err(CoreError::Membership(format!(
                "unexpected join response: {other:?}"
            )));
        }
    };

    // Validate + persist genesis.
    let genesis: Genesis = serde_json::from_slice(&accept.genesis_json)
        .map_err(|e| CoreError::Membership(format!("bad genesis from acceptor: {e}")))?;
    genesis.verify()?;
    if genesis.body.mesh_id != token.mesh_id {
        return Err(CoreError::Membership(
            "acceptor's genesis does not match the invite's mesh".into(),
        ));
    }
    genesis.save(&config.data_dir)?;

    // Seed the op-log and confirm we made it in.
    let engine = StateEngine::open(&config.data_dir, identity.node.clone(), genesis.clone())?;
    engine.ingest(accept.ops)?;
    let me = identity.node.node_id();
    if !engine.read(|s| s.is_active_member(&me)) {
        return Err(CoreError::Membership(
            "join reply did not include our membership — try again".into(),
        ));
    }
    let members = engine.read(|s| s.members.len());
    conn.close(0u32.into(), b"joined");
    Ok(JoinReport {
        mesh_name: genesis.body.mesh_name,
        accepted_by: ack.node_id,
        members,
    })
}

/// A bootstrap can outrun its own delivery: the acceptor keeps authoring
/// while it serves, so `more` can stay true for a genuinely long time. It
/// cannot stay true *forever* — authoring is human-paced and draining is
/// not — but a loop with no ceiling is a hang with no message, so it has
/// one, sized far above any real mesh.
const MAX_BOOTSTRAP_ROUNDS: usize = 10_000;

/// Drain the remaining instalments of a chunked snapshot into one accept.
///
/// Buffered rather than ingested as it arrives, deliberately: the joiner
/// has no data dir yet, and the first thing it does with genesis is
/// *verify* it. Writing ops into a store before that check would mean a
/// failed bootstrap leaves a half-built mesh on disk, which is the state
/// `pair_join`'s "remove the data dir to start over" exists to avoid.
async fn pull_snapshot(
    transport: &Transport,
    conn: &quinn::Connection,
    first: SnapshotChunk,
) -> Result<JoinAcceptV2> {
    let genesis_json = first.genesis_json;
    let mut ops = first.ops;
    let mut more = first.more;
    let mut progress = SnapshotProgress::default();
    progress.observe(&ops);

    let mut rounds = 0usize;
    while more {
        rounds += 1;
        if rounds > MAX_BOOTSTRAP_ROUNDS {
            return Err(CoreError::Membership(format!(
                "the acceptor is still sending history after \
                 {MAX_BOOTSTRAP_ROUNDS} instalments ({} ops so far) — \
                 giving up rather than looping",
                ops.len()
            )));
        }
        let chunk = match transport
            .request_one(conn, &Request::JoinFetch { vv: progress.vv() })
            .await?
        {
            Response::Snapshot(c) => *c,
            Response::Error { message } => return Err(CoreError::Membership(message)),
            other => {
                return Err(CoreError::Membership(format!(
                    "unexpected snapshot response: {other:?}"
                )));
            }
        };
        // An acceptor that says "more" and sends nothing would spin this
        // loop to its ceiling; treat it as the protocol fault it is.
        if chunk.ops.is_empty() && chunk.more {
            return Err(CoreError::Membership(
                "the acceptor reports more history but sent none".into(),
            ));
        }
        progress.observe(&chunk.ops);
        more = chunk.more;
        ops.extend(chunk.ops);
    }
    Ok(JoinAcceptV2 { genesis_json, ops })
}

/// What [`rebuild_oplog`] did.
#[derive(Debug)]
pub struct RebuildReport {
    pub ops: u64,
    pub from: NodeId,
    /// Where the op-log that could not be used was moved.
    pub kept: Option<std::path::PathBuf>,
}

/// Rebuild this node's op-log from a member of its mesh (ENG-13).
///
/// For an op-log that no longer opens, which used to leave a node unable to
/// start and nothing to do about it but a restore. The node keeps its
/// identity and genesis; the broken file is moved aside, not deleted, and
/// put back if the rebuild fails. Everything — this node's own ops
/// included — comes from `from` as a member's chunked bootstrap, and the
/// node then starts under the same hold as a restored one.
///
/// Offline: the daemon must not be running.
pub async fn rebuild_oplog(config: &NodeConfig, from: &[String]) -> Result<RebuildReport> {
    let genesis = Genesis::load(&config.data_dir)?.ok_or_else(|| {
        CoreError::Membership(
            "no mesh here: the data dir has no genesis document — restore a backup, or join".into(),
        )
    })?;
    let layout = config.layout();
    if !layout.identity_dir().join("node.key").exists() {
        return Err(CoreError::Membership(
            "this node has no identity left to rebuild with — restore a backup instead".into(),
        ));
    }
    let identity = Identity::load_or_generate(&config.data_dir)?;
    let me = identity.node.node_id();
    let oplog = layout.state_dir().join("oplog.redb");
    let kept = if oplog.exists() {
        let aside = layout.state_dir().join(format!(
            "oplog.redb.broken-{}",
            mesh_proto::now_micros() / 1_000_000
        ));
        std::fs::rename(&oplog, &aside)?;
        Some(aside)
    } else {
        None
    };

    let local = LocalInfo {
        node_id: me,
        node_name: config.node_name.clone(),
        mesh_id: Some(genesis.body.mesh_id),
        mesh_name: Some(genesis.body.mesh_name.clone()),
    };
    let admission: crate::transport::AdmissionCheck =
        Arc::new(|_, _| Err("a node rebuilding its op-log accepts no connections".into()));
    let (transport, _events) = Transport::bind(
        "0.0.0.0:0".parse().expect("valid bind"),
        identity.node.clone(),
        local,
        admission,
        config.bind_options(),
    )
    .await?;
    let result = async {
        let (conn, ack) = transport.race_join(from).await?;
        if ack.mesh_id != Some(genesis.body.mesh_id) {
            return Err(CoreError::Membership(format!(
                "{} is not a member of this node's mesh",
                ack.node_name
            )));
        }
        let first = match transport
            .request_one(&conn, &Request::JoinFetch { vv: Vec::new() })
            .await?
        {
            Response::Snapshot(chunk) => *chunk,
            Response::Error { message } => return Err(CoreError::Membership(message)),
            other => {
                return Err(CoreError::Membership(format!(
                    "unexpected snapshot response: {other:?}"
                )));
            }
        };
        let accept = pull_snapshot(&transport, &conn, first).await?;
        conn.close(0u32.into(), b"rebuilt");
        crate::state::RestoreHold::place(&config.data_dir, "rebuild")?;
        let engine = StateEngine::open(&config.data_dir, identity.node.clone(), genesis.clone())?;
        let ops = accept.ops.len() as u64;
        engine.ingest(accept.ops)?;
        engine.flush()?;
        if !engine.read(|s| s.is_active_member(&me)) {
            return Err(CoreError::Membership(format!(
                "{} does not know this node as a member of the mesh",
                ack.node_name
            )));
        }
        Ok(RebuildReport {
            ops,
            from: ack.node_id,
            kept: kept.clone(),
        })
    }
    .await;
    transport.close();
    if result.is_err() {
        // Back where the operator started: the partial store goes, the
        // original comes back, and so does the start it could not make.
        let _ = std::fs::remove_file(&oplog);
        let _ = std::fs::remove_file(crate::state::RestoreHold::path(&config.data_dir));
        if let Some(aside) = &kept {
            let _ = std::fs::rename(aside, &oplog);
        }
    }
    result
}

/// Revoke a node with a client-signed, admin-authorized revocation.
/// Terminal; the merge rule verifies the signature and role on every node.
pub fn revoke_node(
    engine: &StateEngine,
    transport: &Transport,
    revocation: NodeRevocation,
) -> Result<()> {
    if revocation.node_id == engine.node_id() {
        return Err(CoreError::Membership(
            "refusing to revoke this node itself".into(),
        ));
    }
    let target = revocation.node_id;
    engine.author_op(OpBody::MemberRevoke { revocation })?;
    transport.drop_conn(&target);
    Ok(())
}

// ---------------------------------------------------------------------------
// Target selectors (MASTERPLAN §10.1 — M1 subset)
// ---------------------------------------------------------------------------

/// `any` | `<name>` | `name=<n>` | `tag=<t>` | `id=<mw1...prefix>` |
/// `fact.<key><op><value>`, joined by `,` (all of) and `|` (any of)
///
/// ## Why adding a variant here is free (M18 §4.7)
///
/// **`Selector` is not a wire type.** It derives `Debug, Clone, PartialEq`
/// and is parsed from strings at every use site, so a new variant changes
/// no encoding anywhere. What *does* travel is the selector **string**,
/// inside signed structs — `Grant::nodes`, `PolicyMatch::nodes` — and those
/// degrade **closed** on a node that has no code for the new form: a
/// pre-M18 node handed `fact.kernel_release<6.1` falls through to
/// `Selector::Name("fact.kernel_release<6.1")` and matches **no** member.
///
/// A grant written with a fact selector is therefore *narrower* on an old
/// node, which is the safe direction. A policy written with one *matches
/// less*, which is the unsafe direction — and that is carried as a risk
/// with `doctor` warning about it, rather than papered over here.
#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    Any,
    Name(String),
    Tag(String),
    IdPrefix(String),
    /// Every part must match — `tag=gpu, tag=canary` (M24 §3.3).
    ///
    /// The form a canary ring is written in: the collector runs where it
    /// belongs *and* where the ring is, and widening it is deleting
    /// `, tag=canary`. Adding this variant is free for exactly the reason
    /// the type doc gives — it degrades **closed** on a node with no code
    /// for it, which for a definition that executes unprompted is the only
    /// safe direction.
    All(Vec<Selector>),
    /// Any part may match — `name=anvil|name=edge-7` (M24 §10).
    ///
    /// The form a **hand-picked set** is written in. Every other variant
    /// names a set by a property its members share; this one is the answer
    /// when the operator's set has no such property — three machines
    /// chosen off a list because those are the three they want upgraded.
    /// Without it the only way to name three nodes was three plans.
    ///
    /// It binds **looser than `,`**, so `tag=gpu, tag=canary|name=anvil`
    /// is *(gpu and canary) or anvil*, which is the precedence every other
    /// language with both operators uses. Like `All`, adding it is free
    /// for the reason the type doc gives: it degrades **closed** on a node
    /// with no code for it, falling through to a `Name` that matches
    /// nothing, so an old node in a hand-picked set refuses rather than
    /// guessing — and for an upgrade plan, refusing is the safe direction.
    AnyOf(Vec<Selector>),
    /// A comparison against the subject's latest fact documents (M18 §4.7).
    ///
    /// Resolution needs [`MeshState`], which is why [`Selector::matches`]
    /// answers `false` for it and [`Selector::matches_in`] is the form
    /// every mesh-side call site uses. That asymmetry is deliberate: a
    /// caller holding only a `MemberInfo` genuinely cannot answer a fact
    /// question, and returning `false` is the fail-closed answer.
    Fact(crate::facts::FactTest),
}

/// The selector written back out, in the grammar it was parsed from.
///
/// `Debug` was standing in for this everywhere a refusal had to name a
/// selector, and `no live node matches selector Tag("edge")` puts the
/// language's syntax in front of somebody who wrote `tag=edge` and is
/// owed that string back. Round-trips through [`Selector::parse`] for
/// every variant; `Any` canonicalises to `any`.
impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Selector::Any => f.write_str("any"),
            Selector::Name(n) => write!(f, "name={n}"),
            Selector::Tag(t) => write!(f, "tag={t}"),
            Selector::IdPrefix(p) => write!(f, "id={p}"),
            Selector::All(parts) => {
                let parts: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
                f.write_str(&parts.join(", "))
            }
            Selector::AnyOf(parts) => {
                let parts: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
                f.write_str(&parts.join("|"))
            }
            Selector::Fact(t) => write!(f, "{t}"),
        }
    }
}

/// Split a selector on `sep`, never inside a `~/pattern/`.
///
/// A pattern is the one place a selector's own punctuation can legitimately
/// appear — `fact.os_id~/^(debian|ubuntu)$/` — and splitting it on the pipe
/// would turn one test into two broken ones. A `\/` inside a pattern is an
/// escaped slash and does not end it.
fn split_outside_patterns(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_pattern = false;
    let mut escaped = false;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if in_pattern {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '/' {
                in_pattern = false;
            }
            continue;
        }
        if c == '~' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            chars.next();
            in_pattern = true;
        } else if c == sep {
            parts.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Does this atom ask about a fact — `fact.…`, or the negated set test?
fn is_fact_atom(s: &str) -> bool {
    s.starts_with(mesh_proto::FACT_SELECTOR_PREFIX)
        || s.strip_prefix('!').is_some_and(|rest| {
            rest.trim_start()
                .starts_with(mesh_proto::FACT_SELECTOR_PREFIX)
        })
}

impl Selector {
    /// Say what is wrong with a selector, while somebody is watching.
    ///
    /// [`Selector::parse`] cannot fail — a malformed `fact.…` lands on a
    /// `Name` that matches nothing, which is the right answer on a node
    /// that meets one it cannot read. It is the wrong answer for the person
    /// who just typed it into a deployment file: that file would publish,
    /// and its supervisor would quietly place nothing. So the places a
    /// selector is *written* ask here first.
    pub fn check(s: &str) -> std::result::Result<(), String> {
        for any in split_outside_patterns(s, '|') {
            for atom in split_outside_patterns(any, ',') {
                let atom = atom.trim();
                if is_fact_atom(atom) {
                    crate::facts::FactTest::parse(atom)?;
                } else if atom.starts_with('!') {
                    return Err(format!(
                        "{atom:?}: only a fact can be negated, as `!fact.<key>?` — a tag or a \
                         name cannot"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn parse(s: &str) -> Selector {
        let s = s.trim();
        if s.is_empty() || s == "any" {
            return Selector::Any;
        }
        // A pipe is disjunction, and it is checked before the comma
        // because it binds looser: `a, b|c` is *(a and b) or c*.
        let any_of = split_outside_patterns(s, '|');
        if any_of.len() > 1 {
            let parts: Vec<Selector> = any_of
                .into_iter()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(Selector::parse)
                .collect();
            return match parts.len() {
                0 => Selector::Any,
                1 => parts.into_iter().next().unwrap_or(Selector::Any),
                _ => Selector::AnyOf(parts),
            };
        }
        // A comma is conjunction, and it is checked before every other
        // form so that no single-selector grammar can ever contain one.
        let all = split_outside_patterns(s, ',');
        if all.len() > 1 {
            let parts: Vec<Selector> = all
                .into_iter()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(Selector::parse)
                .collect();
            return match parts.len() {
                0 => Selector::Any,
                1 => parts.into_iter().next().unwrap_or(Selector::Any),
                _ => Selector::All(parts),
            };
        }
        if let Some(v) = s.strip_prefix("name=") {
            return Selector::Name(v.to_string());
        }
        if let Some(v) = s.strip_prefix("tag=") {
            return Selector::Tag(v.to_string());
        }
        if let Some(v) = s.strip_prefix("id=") {
            return Selector::IdPrefix(v.to_string());
        }
        // A malformed `fact.…` falls through to `Name`, which matches
        // nothing — the same fail-closed landing a pre-M18 node makes, so
        // a typo behaves identically to an old build rather than throwing
        // somewhere a selector cannot report an error.
        if is_fact_atom(s)
            && let Ok(test) = crate::facts::FactTest::parse(s)
        {
            return Selector::Fact(test);
        }
        if s.starts_with("mw1") {
            return Selector::IdPrefix(s.to_string());
        }
        Selector::Name(s.to_string())
    }

    /// Does this selector name `m`, judged on the member record alone?
    ///
    /// A `fact.*` selector answers **false** here, always: facts live in
    /// replicated state, not on the member record, and a caller that has
    /// only the record cannot know. Fail closed — a selector that guessed
    /// `true` would run something everywhere.
    pub fn matches(&self, m: &MemberInfo) -> bool {
        match self {
            Selector::Any => true,
            Selector::Name(n) => m.name == *n,
            Selector::Tag(t) => m.tags.iter().any(|x| x == t),
            Selector::IdPrefix(p) => m.node_id.to_display_string().starts_with(p.as_str()),
            // A conjunction containing a fact test answers false here, by
            // the same fail-closed rule its member does.
            Selector::All(parts) => parts.iter().all(|p| p.matches(m)),
            Selector::AnyOf(parts) => parts.iter().any(|p| p.matches(m)),
            Selector::Fact(_) => false,
        }
    }

    /// [`Selector::matches`] with the state a `fact.*` comparison needs.
    ///
    /// A node with no document for the key matches nothing, which is the
    /// property the exit demo turns on: `--on 'fact.os_id=debian'` hits
    /// exactly the Debian boxes, and a node that has not said what it runs
    /// is not one of them.
    pub fn matches_in(&self, state: &MeshState, m: &MemberInfo) -> bool {
        if let Selector::All(parts) = self {
            return parts.iter().all(|p| p.matches_in(state, m));
        }
        if let Selector::AnyOf(parts) = self {
            return parts.iter().any(|p| p.matches_in(state, m));
        }
        let Selector::Fact(test) = self else {
            return self.matches(m);
        };
        // A node with no documents at all has no value for any key — which
        // fails every test except `!fact.<key>?`, the one that asks exactly
        // that.
        let values = state
            .facts
            .get(&mesh_proto::FactSubject::Node(m.node_id))
            .into_iter()
            .flat_map(|by_collector| by_collector.values())
            .filter_map(|entry| entry.get(&test.key));
        test.matches_values(values)
    }

    /// Does this selector need replicated state to answer?
    ///
    /// What `doctor` asks when deciding whether a live policy's selector
    /// would degrade on an older member (§4.7, R-5).
    pub fn needs_facts(&self) -> bool {
        match self {
            Selector::Fact(_) => true,
            Selector::All(parts) | Selector::AnyOf(parts) => {
                parts.iter().any(Selector::needs_facts)
            }
            _ => false,
        }
    }

    /// Does this selector reach every member, however it is written?
    ///
    /// `any` is not the only spelling of *everything*: `any|name=anvil` is
    /// the same set with a decoration on it, and a refusal that tested the
    /// top-level variant alone would wave it through. Anything that
    /// answers true here is a whole-mesh selector, and the one place that
    /// matters most is the upgrade driver's refusal of `--on any`.
    pub fn is_unbounded(&self) -> bool {
        match self {
            Selector::Any => true,
            // A conjunction is bounded as soon as *one* part bounds it.
            Selector::All(parts) => parts.iter().all(Selector::is_unbounded),
            Selector::AnyOf(parts) => parts.iter().any(Selector::is_unbounded),
            _ => false,
        }
    }
}

/// One machine's value of a placement attribute (Phase C, E7/E8): `name`,
/// `tag.<key>` for a `<key>=<value>` tag, or `fact.<key>`.
///
/// `None` when the machine has none — which `distinct_property` reads as
/// "not a candidate" and `spread` as "the last place to go". For a fact, the
/// first collector by name that has the key answers, so every node reads the
/// same value out of the same state.
pub fn node_attribute(state: &MeshState, info: &MemberInfo, attribute: &str) -> Option<String> {
    if attribute == "name" {
        return Some(info.name.clone());
    }
    if let Some(key) = attribute.strip_prefix("tag.") {
        return info.tags.iter().find_map(|t| {
            t.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        });
    }
    let key = attribute.strip_prefix(mesh_proto::FACT_SELECTOR_PREFIX)?;
    state
        .facts
        .get(&mesh_proto::FactSubject::Node(info.node_id))?
        .values()
        .find_map(|entry| entry.get(key))
}

/// Resolve a selector to candidate members, filtered by liveness. Members
/// carrying the reserved `no-execute` tag are never placement candidates
/// (MASTERPLAN D6 — Director's non-executing peers), and neither are
/// members announcing `draining` (MASTERPLAN §12).
/// Deterministic order: self first, then by node id.
pub fn resolve_targets(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
) -> Vec<MemberInfo> {
    resolve_feasible(
        state,
        selector,
        self_id,
        liveness_of,
        &Requirements::default(),
        |_| None,
    )
}

/// [`resolve_targets`] plus the resource feasibility filter (M9 §5.3).
///
/// Feasibility runs *before* scoring, and it is a filter rather than a
/// penalty on purpose: a node without 16 GB is not a bad candidate for a
/// 16 GB task, it is not a candidate. Two ceilings apply — the node's
/// advertised hardware, which is permanent, and its live free memory and
/// disk, which is not. Both are neutral when unknown, so a node that
/// advertises nothing stays eligible and finds out at admission, where the
/// authoritative check lives (`jobs::accept`).
pub fn resolve_feasible(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    reqs: &Requirements,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> Vec<MemberInfo> {
    resolve_feasible_with_devices(
        state,
        selector,
        self_id,
        liveness_of,
        reqs,
        None,
        pressure_of,
    )
}

/// [`resolve_feasible`] with the device filter (C5 P3).
///
/// The device term is filtered from the coarse `cap.gpus` / `cap.vram_mb`
/// tags rather than from the replicated inventory, which is exactly what
/// those tags are for (§9.1): a selector that had to parse an inventory per
/// candidate would read a device list for every CPU box in the mesh to
/// answer "has a GPU".
///
/// **One rule is inverted here and only here.** Everywhere else in the
/// capacity model an unmeasured dimension is neutral. A node that
/// advertises *no* device tags is not a candidate for a device request,
/// because an absent `cap.gpus` on a node running this build means the
/// machine has no GPU — not that nobody looked. Treating it as neutral
/// would send every GPU job to every CPU box in the mesh and let admission
/// refuse it N times.
/// [`resolve_feasible_with_devices`], and a tally of everything it dropped
/// (N1 P2).
///
/// **This is the filter.** `resolve_feasible_with_devices` is a thin call
/// into it that throws the tally away, so there is exactly one place where
/// "is this node a candidate" is decided. The N-track's landmine — *do not
/// build a second explainer* — is enforced structurally here rather than
/// by a comment asking the next person to remember.
///
/// A node is counted against **at most one** reason: the walk stops at the
/// first gate it fails, in the order the gates apply. That is what makes
/// the counts a partition of the mesh, so "excluded 4 of 7" is arithmetic
/// a reader can check rather than a set of overlapping claims.
#[allow(clippy::too_many_arguments)]
pub fn audit_feasible_with_devices(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    reqs: &Requirements,
    devices: Option<&mesh_proto::DeviceRequest>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> (Vec<MemberInfo>, mesh_proto::PlacementRefusal) {
    audit_feasible_full(
        state,
        selector,
        self_id,
        liveness_of,
        reqs,
        devices,
        &mesh_proto::PortRequests::default(),
        &mesh_proto::VolumeRequests::default(),
        // No runtime stated. This wrapper is for callers that have
        // requirements and devices and nothing else; the ones that hold a
        // job's environment should call `audit_feasible_for_env`, which
        // derives every filter including this one.
        None,
        pressure_of,
    )
}

/// [`audit_feasible_with_devices`] with the port filter (N2 P3).
///
/// The filter, and the only one — the two functions above are thin calls
/// into this, so there is exactly one place where "is this node a
/// candidate" is decided.
#[allow(clippy::too_many_arguments)]
pub fn audit_feasible_full(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    reqs: &Requirements,
    devices: Option<&mesh_proto::DeviceRequest>,
    ports: &mesh_proto::PortRequests,
    volumes: &mesh_proto::VolumeRequests,
    runtime: Option<mesh_proto::Runtime>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> (Vec<MemberInfo>, mesh_proto::PlacementRefusal) {
    use mesh_proto::Exclusion;

    let mut out: Vec<MemberInfo> = Vec::new();
    let mut tally: [u32; REASONS.len()] = [0; REASONS.len()];
    let mut considered = 0u32;
    let mut most_free_mem_mb: Option<u64> = None;
    let mut most_free_disk_mb: Option<u64> = None;
    let mut most_cores: Option<u32> = None;
    let bump = |r: Exclusion, tally: &mut [u32; REASONS.len()]| tally[r as usize] += 1;

    for m in state.members.values() {
        considered += 1;
        let info = &m.info;
        if m.left || state.revoked.contains_key(&info.node_id) {
            bump(Exclusion::Gone, &mut tally);
            continue;
        }
        // The owner's veto, before anything the node says about itself
        // (`MeshState::may_execute`). A machine can offer its cores; only
        // the mesh's admins decide whether the offer is taken.
        if !state.may_execute(&info.node_id) {
            bump(Exclusion::NotPermitted, &mut tally);
            continue;
        }
        // Draining covers both kinds (Phase C, E9): a node shutting down, and
        // one an admin is emptying with `nodes drain` while it stays up.
        if info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG)
            || mesh_proto::drain_of(&info.tags).is_some()
        {
            bump(Exclusion::NotExecuting, &mut tally);
            continue;
        }
        if !selector.matches_in(state, info) {
            bump(Exclusion::Selector, &mut tally);
            continue;
        }
        if info.node_id != self_id && !matches!(liveness_of(&info.node_id), Liveness::Alive) {
            bump(Exclusion::NotAlive, &mut tally);
            continue;
        }
        if !reqs.is_unstated() {
            let cap = mesh_proto::Capacity::from_tags(&info.tags);
            // Permanent before transient: a box that is too small to ever
            // run this is not "busy", and telling somebody to wait for it
            // wastes their afternoon.
            if !reqs.fits_capacity(cap.as_ref()) {
                if let Some(c) = cap.as_ref().map(|c| c.cores).filter(|c| *c > 0) {
                    most_cores = Some(most_cores.map_or(c, |best: u32| best.max(c)));
                }
                bump(Exclusion::Hardware, &mut tally);
                continue;
            }
            let pressure = pressure_of(&info.node_id);
            if !reqs.fits_pressure(pressure.as_ref().map(|s| &s.base)) {
                if let Some(p) = pressure.as_ref().map(|s| &s.base) {
                    if let Some(free) = p.mem_available_mb {
                        most_free_mem_mb =
                            Some(most_free_mem_mb.map_or(free, |best: u64| best.max(free)));
                    }
                    if let Some(free) = p.disk_free_mb {
                        most_free_disk_mb =
                            Some(most_free_disk_mb.map_or(free, |best: u64| best.max(free)));
                    }
                }
                bump(Exclusion::Busy, &mut tally);
                continue;
            }
        }
        if !devices.is_none_or(|req| req.fits_tags(&info.tags)) {
            bump(Exclusion::Devices, &mut tally);
            continue;
        }
        // N2 P3. Only the *range* is checked here, never which ports are
        // free: that is live, node-local state this side cannot see, and a
        // static port already taken is settled where it is knowable — at
        // admission, which answers `Busy` and lets the job spill to the
        // next candidate. Filtering on a guess about a remote node's port
        // table would be wrong more often than it was right.
        if !ports.is_empty() && mesh_proto::PortRange::from_tags(&info.tags).is_none() {
            bump(Exclusion::Ports, &mut tally);
            continue;
        }
        // T3. Can this machine run this kind of work at all?
        //
        // **The last gate, deliberately.** It is the only one whose answer
        // is "no, and waiting will not help" for a reason the operator can
        // fix in one command, so it reads best as the final word rather
        // than as one of several things wrong. And it is cheap: a tag
        // lookup, after every filter that might already have excluded the
        // node for a reason closer to what was asked.
        //
        // A node that announces nothing is *unknown*, and unknown stays a
        // candidate — see `mesh_proto::runtimes::from_tags` for why this
        // one inverts the rule ports and volumes follow.
        if let Some(want) = runtime
            && !mesh_proto::runtimes::tags_can_run(&info.tags, want)
        {
            bump(Exclusion::Runtime, &mut tally);
            continue;
        }
        // N5. Only that the node *offers* the name — whether the claim is
        // free is live, node-local state this side cannot see, and it is
        // settled where it is knowable: at admission, which answers `Busy`
        // and lets the job spill. The same split ports already make.
        if !volumes.is_empty() {
            let offered = mesh_proto::volumes::from_tags(&info.tags);
            if !volumes
                .names()
                .iter()
                .all(|want| offered.iter().any(|have| have == want))
            {
                bump(Exclusion::Volumes, &mut tally);
                continue;
            }
        }
        out.push(info.clone());
    }
    out.sort_by_key(|m| (m.node_id != self_id, m.node_id));
    let refusal = mesh_proto::PlacementRefusal {
        selector: selector.to_string(),
        considered,
        feasible: out.len() as u32,
        excluded: REASONS
            .iter()
            .filter(|r| tally[**r as usize] > 0)
            .map(|r| (*r, tally[*r as usize]))
            .collect(),
        needs_cpus: reqs.cpus,
        needs_mem_mb: reqs.mem_mb,
        needs_disk_mb: reqs.disk_mb,
        most_free_mem_mb,
        most_free_disk_mb,
        most_cores,
        needs_ports: ports.len() as u32,
        needs_volumes: volumes.names().iter().map(|s| s.to_string()).collect(),
        needs_runtime: runtime,
    };
    (out, refusal)
}

/// Every exclusion reason, in the order a refusal lists them.
///
/// **Module-level so the tally array and this list cannot disagree about
/// their length.** They were two literals — `[u32; 10]` and a ten-element
/// `const` inside the function — and an eleventh reason meant editing both,
/// with a silent out-of-bounds index waiting for whoever edited one.
const REASONS: [mesh_proto::Exclusion; 11] = [
    mesh_proto::Exclusion::Gone,
    mesh_proto::Exclusion::NotPermitted,
    mesh_proto::Exclusion::NotExecuting,
    mesh_proto::Exclusion::Selector,
    mesh_proto::Exclusion::NotAlive,
    mesh_proto::Exclusion::Hardware,
    mesh_proto::Exclusion::Busy,
    mesh_proto::Exclusion::Devices,
    mesh_proto::Exclusion::Volumes,
    mesh_proto::Exclusion::Ports,
    mesh_proto::Exclusion::Runtime,
];

#[allow(clippy::too_many_arguments)]
pub fn resolve_feasible_with_devices(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    reqs: &Requirements,
    devices: Option<&mesh_proto::DeviceRequest>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> Vec<MemberInfo> {
    audit_feasible_with_devices(
        state,
        selector,
        self_id,
        liveness_of,
        reqs,
        devices,
        pressure_of,
    )
    .0
}

/// The candidates for a job with **this environment** — every filter the
/// `MESHWARE_TASK_*` keys can ask for, derived in one place.
///
/// **Prefer this wherever the caller has the job's environment.** The
/// requirement keys all travel the same carrier, and reading some of them
/// at a call site while defaulting the rest is how N2's port filter and
/// N5's volume filter came to be written, tested, and then never consulted
/// by a dispatch: `audit_feasible_full` took both and every caller passed
/// `Default::default()`, so a job claiming a volume no node lends was
/// refused with *"every matching node is saturated"* — pointing the reader
/// at load on machines that were never candidates.
pub fn resolve_feasible_for_env(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    env: &[(String, String)],
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> Vec<MemberInfo> {
    audit_feasible_for_env(state, selector, self_id, liveness_of, env, pressure_of).0
}

/// [`resolve_feasible_for_env`], keeping the tally that says *why*.
pub fn audit_feasible_for_env(
    state: &MeshState,
    selector: &Selector,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> Liveness,
    env: &[(String, String)],
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
) -> (Vec<MemberInfo>, mesh_proto::PlacementRefusal) {
    audit_feasible_full(
        state,
        selector,
        self_id,
        liveness_of,
        &Requirements::from_env(env),
        mesh_proto::DeviceRequest::from_env(env).as_ref(),
        &mesh_proto::PortRequests::from_env(env),
        &mesh_proto::VolumeRequests::from_env(env),
        mesh_proto::runtimes::from_env(env),
        pressure_of,
    )
}

// ---------------------------------------------------------------------------
// N1 P2 — the audit's properties
// ---------------------------------------------------------------------------

#[cfg(test)]
mod audit_tests {
    use super::*;
    use mesh_proto::{Capacity, Exclusion, Hlc, MemberInfo, PressureSnapshot, SystemSnapshot};

    fn node_id(byte: u8) -> NodeId {
        NodeId([byte; 32])
    }

    /// A member with `cap.*` tags for `cap`, plus whatever else is asked.
    fn member(
        byte: u8,
        name: &str,
        cap: Option<Capacity>,
        extra: &[&str],
    ) -> crate::state::MemberEntry {
        let mut tags: Vec<String> = cap.map(|c| c.to_tags()).unwrap_or_default();
        tags.extend(extra.iter().map(|t| t.to_string()));
        crate::state::MemberEntry {
            info: MemberInfo {
                node_id: node_id(byte),
                name: name.to_string(),
                addrs: Vec::new(),
                tags: tags.clone(),
                slots: 1,
                joined_at_micros: 0,
            },
            announced_tags: tags,
            added_hlc: Hlc::default(),
            updated_hlc: Hlc::default(),
            left: false,
            left_hlc: None,
        }
    }

    fn pressure(mem_free_mb: u64) -> SystemSnapshot {
        SystemSnapshot::from_pressure(PressureSnapshot {
            load1: None,
            mem_available_mb: Some(mem_free_mb),
            disk_free_mb: None,
            psi_cpu: None,
            psi_io: None,
            psi_mem: None,
            running: 0,
            queued: 0,
            hlc: Hlc::default(),
        })
    }

    fn box_of(cores: u32, mem_mb: u64) -> Capacity {
        Capacity {
            cores,
            mem_mb,
            disk_mb: 100_000,
            arch: "x86_64".into(),
            ports: Some(mesh_proto::PortRange::default()),
            volumes: Vec::new(),
            // Unknown, which is neutral: these fixtures predate T3 and must
            // keep behaving as they did, which is exactly the property
            // `a_node_that_says_nothing_can_still_run_anything` pins.
            runtimes: Vec::new(),
        }
    }

    /// Three edge boxes, one of which is small, plus three non-edge ones.
    fn mesh() -> MeshState {
        let mut s = MeshState::default();
        for (b, name, cap, extra) in [
            (1u8, "edge-1", box_of(8, 16_384), &["edge"][..]),
            (2, "edge-2", box_of(8, 16_384), &["edge"]),
            (3, "edge-tiny", box_of(2, 1_024), &["edge"]),
            (4, "core-1", box_of(64, 262_144), &["core"]),
            (5, "core-2", box_of(64, 262_144), &["core"]),
            (6, "core-3", box_of(64, 262_144), &["core"]),
        ] {
            s.members
                .insert(node_id(b), member(b, name, Some(cap), extra));
        }
        s
    }

    // ---- T3: runtimes as a placement gate ---------------------------

    /// A mesh where one node can run containers and one cannot, and both
    /// say so — the case the whole track exists for.
    fn runtime_mesh() -> MeshState {
        use mesh_proto::Runtime;
        let mut s = MeshState::default();
        let mut with_engine = box_of(8, 16_384);
        with_engine.runtimes = vec![Runtime::Exec, Runtime::Bash, Runtime::Container];
        let mut without = box_of(8, 16_384);
        without.runtimes = vec![Runtime::Exec, Runtime::Bash];
        s.members
            .insert(node_id(1), member(1, "has-podman", Some(with_engine), &[]));
        s.members
            .insert(node_id(2), member(2, "no-engine", Some(without), &[]));
        s
    }

    fn feasible_for(s: &MeshState, runtime: mesh_proto::Runtime) -> (Vec<String>, u32) {
        let env = vec![mesh_proto::runtimes::to_env(runtime)];
        let (out, refusal) = audit_feasible_for_env(
            s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &env,
            |_| None,
        );
        let excluded = refusal
            .excluded
            .iter()
            .find(|(r, _)| *r == mesh_proto::Exclusion::Runtime)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        (out.into_iter().map(|m| m.name).collect(), excluded)
    }

    /// **The bug this closes.** A container job was placed wherever the
    /// hardware fit and discovered at spawn that the node had no engine —
    /// an error with no scheduling consequence and nothing anywhere saying
    /// *this machine cannot do that*.
    #[test]
    fn a_container_job_only_goes_where_a_container_can_run() {
        let s = runtime_mesh();
        let (names, excluded) = feasible_for(&s, mesh_proto::Runtime::Container);
        assert_eq!(names, vec!["has-podman".to_string()]);
        assert_eq!(
            excluded, 1,
            "the node without an engine is counted, by name"
        );

        // …and work both nodes can run still reaches both.
        let (names, excluded) = feasible_for(&s, mesh_proto::Runtime::Bash);
        assert_eq!(names.len(), 2);
        assert_eq!(excluded, 0);
    }

    /// **D-T3, as a test rather than as a comment somebody can contradict.**
    ///
    /// A node running a build from before T3 announces no `cap.rt`. It may
    /// have a container plugin configured and be running containers
    /// perfectly. Excluding it would mean upgrading one node silently
    /// shrinks the mesh's container capacity — a deployment shedding
    /// replicas with nothing anywhere saying why.
    #[test]
    fn a_node_too_old_to_announce_runtimes_is_still_a_candidate() {
        let mut s = runtime_mesh();
        // The pre-T3 shape: capacity tags, no `cap.rt` at all.
        s.members.insert(
            node_id(3),
            member(3, "old-build", Some(box_of(8, 16_384)), &[]),
        );
        assert!(
            !s.members[&node_id(3)]
                .info
                .tags
                .iter()
                .any(|t| t.starts_with("cap.rt")),
            "the fixture must actually be the old shape"
        );

        let (names, _) = feasible_for(&s, mesh_proto::Runtime::Container);
        assert!(
            names.contains(&"old-build".to_string()),
            "unknown must stay a candidate, or an upgrade shrinks the mesh"
        );
        assert!(
            !names.contains(&"no-engine".to_string()),
            "…but a stated no is still no"
        );
    }

    /// Work that names no runtime is filtered by nothing, which is what
    /// every caller written before T3 expects.
    #[test]
    fn work_that_states_no_runtime_is_not_filtered() {
        let s = runtime_mesh();
        let (out, refusal) = audit_feasible_for_env(
            &s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &[],
            |_| None,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(refusal.needs_runtime, None);
    }

    /// The refusal has to say what is missing **on the machine**, not what
    /// the file asked for: "cannot run container work" sends somebody to
    /// `dnf install podman`, where "needs runtime container" sends them to
    /// re-read a file that was never wrong.
    #[test]
    fn the_refusal_names_the_runtime_and_what_to_do() {
        let mut s = runtime_mesh();
        s.members.remove(&node_id(1));
        let env = vec![mesh_proto::runtimes::to_env(mesh_proto::Runtime::Container)];
        let (out, refusal) = audit_feasible_for_env(
            &s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &env,
            |_| None,
        );
        assert!(out.is_empty());
        let lines = refusal.lines().join(" | ");
        assert!(lines.contains("container"), "{lines}");
        assert!(
            lines.contains("podman") || lines.contains("docker"),
            "{lines}"
        );
        assert!(
            !refusal.transient(),
            "installing an engine is not something waiting fixes"
        );
    }

    /// The property the counts depend on: every node lands in exactly one
    /// bucket, so "excluded 4 of 7" is arithmetic rather than a claim.
    #[test]
    fn every_node_is_counted_against_exactly_one_reason() {
        let s = mesh();
        let (feasible, refusal) = audit_feasible_with_devices(
            &s,
            &Selector::parse("tag=edge"),
            node_id(99),
            |_| Liveness::Alive,
            &Requirements::default(),
            None,
            |_| None,
        );
        let excluded: u32 = refusal.excluded.iter().map(|(_, n)| n).sum();
        assert_eq!(refusal.considered, 6);
        assert_eq!(refusal.feasible, feasible.len() as u32);
        assert_eq!(
            excluded + refusal.feasible,
            refusal.considered,
            "the buckets must partition the mesh: {:?}",
            refusal.excluded
        );
        assert_eq!(refusal.excluded, vec![(Exclusion::Selector, 3)]);
    }

    /// The audit *is* the filter. If these ever disagree, the refusal a
    /// person reads has stopped describing the decision the scheduler made
    /// — which is the whole failure this design exists to prevent.
    #[test]
    fn the_audit_and_the_filter_return_the_same_nodes() {
        let s = mesh();
        for sel in ["any", "tag=edge", "tag=core", "name=edge-1", "tag=nope"] {
            let selector = Selector::parse(sel);
            let reqs = Requirements {
                cpus: 4,
                mem_mb: 8_192,
                disk_mb: 0,
            };
            let filtered = resolve_feasible_with_devices(
                &s,
                &selector,
                node_id(99),
                |_| Liveness::Alive,
                &reqs,
                None,
                |_| None,
            );
            let (audited, _) = audit_feasible_with_devices(
                &s,
                &selector,
                node_id(99),
                |_| Liveness::Alive,
                &reqs,
                None,
                |_| None,
            );
            let names = |v: &[MemberInfo]| v.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
            assert_eq!(names(&filtered), names(&audited), "selector {sel}");
        }
    }

    /// A claim filters on the name the node *announces*, and the refusal
    /// says so (N5).
    ///
    /// This is the half of the volume filter that lives on the dispatch
    /// side. It deliberately does **not** ask whether the claim is free —
    /// that is live, node-local state, settled at admission — only whether
    /// this node lends the name at all.
    #[test]
    fn a_volume_claim_excludes_the_nodes_that_do_not_lend_it() {
        let mut cap = box_of(8, 16_384);
        cap.volumes = vec!["data".to_string()];
        let mut s = MeshState::default();
        s.members
            .insert(node_id(1), member(1, "has-data", Some(cap), &[]));
        s.members.insert(
            node_id(2),
            member(2, "no-data", Some(box_of(8, 16_384)), &[]),
        );

        let env = mesh_proto::VolumeRequests(vec![mesh_proto::VolumeRequest {
            name: "data".to_string(),
            path: String::new(),
            mode: mesh_proto::volumes::Mode::ReadWrite,
        }])
        .to_env();
        let (feasible, refusal) = audit_feasible_for_env(
            &s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &env,
            |_| None,
        );
        assert_eq!(
            feasible.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["has-data"]
        );
        assert_eq!(refusal.feasible, 1);
        assert!(
            refusal.needs_volumes.iter().any(|v| v == "data"),
            "the refusal does not name the volume: {refusal:?}"
        );
        assert!(
            refusal
                .lines()
                .iter()
                .any(|l| l.to_lowercase().contains("volume")),
            "no line mentions volumes: {:?}",
            refusal.lines()
        );

        // An empty claim filters nothing — the property that keeps this
        // from quietly disqualifying every job that wants no volume.
        let (all, _) = audit_feasible_for_env(
            &s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &[],
            |_| None,
        );
        assert_eq!(all.len(), 2);
    }

    /// "Too small to ever run this" and "full right now" are different
    /// sentences, and only one of them is worth waiting out.
    #[test]
    fn hardware_and_pressure_are_different_refusals() {
        let s = mesh();
        // 8 GB wanted: `edge-tiny` can never take it, the other two could
        // but have 384 MB free.
        let reqs = Requirements {
            cpus: 0,
            mem_mb: 8_192,
            disk_mb: 0,
        };
        let (feasible, refusal) = audit_feasible_with_devices(
            &s,
            &Selector::parse("tag=edge"),
            node_id(99),
            |_| Liveness::Alive,
            &reqs,
            None,
            |_| Some(pressure(384)),
        );
        assert!(feasible.is_empty());
        assert_eq!(
            refusal.excluded,
            vec![
                (Exclusion::Selector, 3),
                (Exclusion::Hardware, 1),
                (Exclusion::Busy, 2)
            ]
        );
        // Not transient: one of those nodes will never be big enough, so
        // telling somebody to wait would waste their afternoon.
        assert!(!refusal.transient());
        assert_eq!(refusal.most_free_mem_mb, Some(384));
        assert!(
            refusal.lines().iter().any(|l| l.contains("most free 384M")),
            "{:?}",
            refusal.lines()
        );
    }

    /// A mesh that is merely busy says so, and says it will fix itself.
    #[test]
    fn a_purely_transient_refusal_says_it_will_place_itself() {
        let mut s = MeshState::default();
        for b in 1..=3u8 {
            s.members.insert(
                node_id(b),
                member(b, &format!("box-{b}"), Some(box_of(8, 16_384)), &[]),
            );
        }
        let reqs = Requirements {
            cpus: 0,
            mem_mb: 8_192,
            disk_mb: 0,
        };
        let (feasible, refusal) = audit_feasible_with_devices(
            &s,
            &Selector::Any,
            node_id(99),
            |_| Liveness::Alive,
            &reqs,
            None,
            |_| Some(pressure(512)),
        );
        assert!(feasible.is_empty());
        assert_eq!(refusal.excluded, vec![(Exclusion::Busy, 3)]);
        assert!(refusal.transient());
        assert!(
            refusal
                .lines()
                .iter()
                .any(|l| l.contains("place itself when a node frees up")),
            "{:?}",
            refusal.lines()
        );
    }

    /// The owner's veto and the node's own tags are separate reasons, and
    /// the veto is reported first because it is the one an operator can
    /// act on.
    #[test]
    fn the_veto_is_counted_before_the_tag() {
        let mut s = mesh();
        s.node_execution.insert(
            node_id(1),
            crate::state::NodeExecutionEntry {
                allowed: false,
                decided_by: mesh_proto::UserId([0; 32]),
                decided_at_micros: 0,
                reason: "held".into(),
                hlc: Hlc::default(),
            },
        );
        s.members
            .get_mut(&node_id(2))
            .expect("edge-2")
            .info
            .tags
            .push(mesh_proto::DRAINING_TAG.to_string());
        let (feasible, refusal) = audit_feasible_with_devices(
            &s,
            &Selector::parse("tag=edge"),
            node_id(99),
            |_| Liveness::Alive,
            &Requirements::default(),
            None,
            |_| None,
        );
        assert_eq!(
            feasible.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["edge-tiny"]
        );
        assert_eq!(
            refusal.excluded,
            vec![
                (Exclusion::NotPermitted, 1),
                (Exclusion::NotExecuting, 1),
                (Exclusion::Selector, 3)
            ]
        );
    }

    /// The selector a refusal names is the one the operator wrote, not
    /// `Tag("edge")`.
    #[test]
    fn a_selector_round_trips_through_its_own_grammar() {
        for text in [
            "any",
            "tag=edge",
            "name=anvil",
            "id=mw1abc",
            "tag=gpu, tag=canary",
            "name=anvil|name=edge-7",
            "fact.os_id=debian",
            // Phase C, E7.
            "fact.kernel_release:version>=6.1",
            "fact.gpu.vendor?",
            "tag=gpu, !fact.gpu.vendor?",
            r"fact.os_id~/^(debian|ubuntu)$/, tag=edge",
        ] {
            let parsed = Selector::parse(text);
            assert_eq!(parsed.to_string(), text, "rendering {text}");
            assert_eq!(
                Selector::parse(&parsed.to_string()),
                parsed,
                "re-parsing {text}"
            );
        }
    }

    /// A pipe or a comma inside a pattern is the pattern's, not the
    /// selector's: the one test stays one test.
    #[test]
    fn a_pattern_is_not_split_on_the_selectors_own_punctuation() {
        let parsed = Selector::parse(r"fact.os_id~/^(debian|ubuntu)$/");
        assert!(matches!(parsed, Selector::Fact(_)), "{parsed:?}");
        let parsed = Selector::parse(r"fact.x~/a\/b|c,d/|name=anvil");
        let Selector::AnyOf(parts) = &parsed else {
            panic!("{parsed:?}");
        };
        assert_eq!(parts.len(), 2);
        assert!(matches!(parts[0], Selector::Fact(_)));
    }

    fn with_fact(s: &mut MeshState, byte: u8, doc: serde_json::Value) {
        s.facts
            .entry(mesh_proto::FactSubject::Node(node_id(byte)))
            .or_default()
            .insert(
                "system".into(),
                crate::state::FactEntry {
                    doc: doc.to_string(),
                    digest: [0; 32],
                    collected_at_micros: 0,
                    author: node_id(byte),
                    updated_hlc: Hlc::default(),
                    parsed: doc,
                },
            );
    }

    /// Phase C, E7, against state: a version floor, a pattern, and the set
    /// tests — where a node that has published nothing is exactly what
    /// `!fact.<key>?` is asking for.
    #[test]
    fn the_new_fact_tests_select_against_state() {
        let mut s = mesh();
        with_fact(
            &mut s,
            1,
            serde_json::json!({"kernel": "6.1.0-18-amd64", "os": "debian"}),
        );
        with_fact(
            &mut s,
            2,
            serde_json::json!({"kernel": "5.15.0", "gpu": {"vendor": "nvidia"}}),
        );
        let names = |sel: &str| -> Vec<String> {
            let sel = Selector::parse(sel);
            let mut out: Vec<String> = s
                .members
                .values()
                .filter(|m| sel.matches_in(&s, &m.info))
                .map(|m| m.info.name.clone())
                .collect();
            out.sort();
            out
        };
        assert_eq!(names("fact.kernel:version>=6.1"), ["edge-1"]);
        assert_eq!(names("fact.os~/^deb/"), ["edge-1"]);
        assert_eq!(names("fact.gpu.vendor?"), ["edge-2"]);
        assert_eq!(
            names("tag=edge, !fact.gpu.vendor?"),
            ["edge-1", "edge-tiny"],
            "a node with no documents has no GPU fact"
        );
        // Judged on the member record alone, a fact test is false — the
        // negated set test included, or a caller without state would run
        // something everywhere.
        let unset = Selector::parse("!fact.gpu.vendor?");
        assert!(!unset.matches(&s.members[&node_id(3)].info));
    }

    #[test]
    fn a_malformed_selector_is_said_where_it_is_written() {
        assert!(Selector::check("tag=edge, fact.kernel:version>=6.1").is_ok());
        assert!(Selector::check(r"fact.os~/^(a|b)$/|name=anvil").is_ok());
        let why = Selector::check("tag=edge, fact.os_id").unwrap_err();
        assert!(why.contains("not a fact test"), "{why}");
        let why = Selector::check("fact.os~/[/").unwrap_err();
        assert!(why.contains("does not compile"), "{why}");
        let why = Selector::check("!tag=gpu").unwrap_err();
        assert!(why.contains("only a fact can be negated"), "{why}");
    }
}
