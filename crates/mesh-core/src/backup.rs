//! Backup & restore (MASTERPLAN §4.5): one snapshot file per node holding
//! the node/user identity seeds, the genesis document, and the complete
//! op-log. Restore rebuilds the data dir and replays the ops — materialized
//! state, secrets (sealed records live in the op-log), services, grants and
//! audit history all come back deterministically. Job spools and content
//! blobs are deliberately excluded: they are large, node-local artifacts
//! that re-replicate from peers (content) or are history (spools).
//!
//! The file embeds private key seeds, so it is written mode 0600 and must
//! be treated like a key file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::identity::{Identity, Keypair};
use crate::state::StateEngine;
use mesh_proto::{Op, OpEnvelope};

/// Format 2 (F0): `ops_b64` is a postcard `Vec<OpEnvelope>`. Format 1 is
/// a postcard `Vec<Op>` and is still readable — a backup taken before the
/// upgrade restores into a 1.7 node, which is the whole point of having
/// one.
pub const FORMAT: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct BackupDoc {
    pub format: u32,
    pub created_at_micros: u64,
    pub mesh_name: String,
    pub node_name: String,
    /// `mesh/genesis.json`, verbatim.
    pub genesis_b64: String,
    /// `identity/node.key` (32-byte Ed25519 seed).
    pub node_key_b64: String,
    /// `identity/user.key`, **verbatim** (ENG-14): a 32-byte seed, a
    /// passphrase-sealed `MWKEY1` file, or an `MWHWK1` reference to a key on
    /// hardware. Empty when the node had none.
    pub user_key_b64: String,
    /// What [`Self::user_key_b64`] holds — `plain`, `sealed`, `hardware`
    /// or `absent`. Informational: the bytes say the same, and a backup
    /// from before this field had one is read from its bytes.
    #[serde(default)]
    pub user_key_kind: String,
    /// The full op-log: postcard `Vec<OpEnvelope>` at [`FORMAT`] 2,
    /// postcard `Vec<Op>` at format 1.
    pub ops_b64: String,
    pub op_count: u64,
    /// `volumes.json`, verbatim — this node's record of the volumes it
    /// provisioned (ENG-27). Empty when it has none.
    ///
    /// **The one thing in the data directory that no peer can give back.**
    /// Everything else a backup restores is either replicated (the op-log)
    /// or re-fetchable (the content store); a volume is a directory on
    /// *this* machine, so its record lives on disk and not in the log —
    /// deliberately, because no other node could verify or act on it. That
    /// is exactly what makes leaving it out of the backup a silent data
    /// loss: restore a node and its provisioned volumes are gone, with the
    /// directories still on the disk and nothing claiming them.
    ///
    /// `#[serde(default)]`, so a backup taken before this field existed
    /// restores as it always did.
    #[serde(default)]
    pub volumes_json_b64: String,
}

#[derive(Debug)]
pub struct BackupReport {
    pub path: PathBuf,
    pub ops: u64,
    pub bytes: u64,
}

/// What a user key file holds, by its own bytes.
fn user_key_kind(bytes: &[u8]) -> Result<&'static str> {
    use crate::keyfile::Stored;
    if bytes.is_empty() {
        return Ok("absent");
    }
    Ok(match Stored::parse_bytes(bytes)? {
        Stored::Plain(_) => "plain",
        Stored::Sealed(_) => "sealed",
        Stored::Hardware(_) => "hardware",
    })
}

fn b64(data: &[u8]) -> String {
    data_encoding::BASE64.encode(data)
}

fn unb64(s: &str, what: &str) -> Result<Vec<u8>> {
    data_encoding::BASE64
        .decode(s.as_bytes())
        .map_err(|e| CoreError::Store(format!("backup field {what} is corrupt: {e}")))
}

/// Write a backup file from already-loaded pieces. Identity seeds are read
/// from `data_dir/identity/` (the on-disk source of truth).
pub fn write_backup(
    out: &Path,
    data_dir: &Path,
    node_name: &str,
    genesis: &Genesis,
    ops: Vec<OpEnvelope>,
) -> Result<BackupReport> {
    let id_dir = Identity::identity_dir(data_dir);
    let node_key = std::fs::read(id_dir.join("node.key"))
        .map_err(|e| CoreError::Store(format!("cannot read node key: {e}")))?;
    // Carried as the file is, whatever form it is in (ENG-14). A protected
    // key used to be copied byte for byte and then refused on restore for
    // not being 32 bytes — a backup that could not be restored, found out
    // during the incident it was taken for.
    let user_key = match std::fs::read(id_dir.join("user.key")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(CoreError::Store(format!("cannot read user key: {e}"))),
    };
    let user_key_kind = user_key_kind(&user_key)?;
    let genesis_json = serde_json::to_vec_pretty(genesis)
        .map_err(|e| CoreError::Store(format!("encode genesis: {e}")))?;
    let op_count = ops.len() as u64;
    let ops_bytes =
        postcard::to_stdvec(&ops).map_err(|e| CoreError::Store(format!("encode ops: {e}")))?;

    let doc = BackupDoc {
        format: FORMAT,
        created_at_micros: mesh_proto::now_micros(),
        mesh_name: genesis.body.mesh_name.clone(),
        node_name: node_name.to_string(),
        genesis_b64: b64(&genesis_json),
        node_key_b64: b64(&node_key),
        user_key_b64: b64(&user_key),
        user_key_kind: user_key_kind.to_string(),
        ops_b64: b64(&ops_bytes),
        op_count,
        volumes_json_b64: b64(&read_volumes(data_dir)?),
    };
    let bytes = serde_json::to_vec_pretty(&doc)
        .map_err(|e| CoreError::Store(format!("encode backup: {e}")))?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    // 0600 **at creation** (ENG-27). This wrote the file and then
    // chmod'ed it, which leaves a window — however short — in which a
    // document holding this node's private key seed and its user's is
    // readable by everything else on the machine. The streaming path
    // beside this one has always opened it correctly; this one did not,
    // and the two write the same secrets.
    {
        use std::io::Write as _;
        let mut file = open_private(out)?;
        file.write_all(&bytes)?;
        file.flush()?;
    }
    Ok(BackupReport {
        path: out.to_path_buf(),
        ops: op_count,
        bytes: bytes.len() as u64,
    })
}

/// A placeholder for the ops field while the rest of the document is
/// serialized. Chosen so that it cannot collide with any real content and
/// needs no JSON escaping.
const OPS_SENTINEL: &str = "@@meshware-backup-ops-go-here@@";

/// Write a backup **without ever holding the log in memory** (ENG-20).
///
/// The old path built four full copies of the op-log to write one: the
/// decoded `Vec<OpEnvelope>`, its postcard re-encoding, the base64 of
/// that, and the JSON document containing it — four to five times the log
/// on a heap, on a node that is very often taking the backup *because*
/// something is already wrong with it. It then wrote the file in place,
/// non-atomically, and chmod'ed it to 0600 afterwards: a crash mid-write
/// left a truncated backup where a good one used to be, and the file was
/// briefly world-readable while holding private key seeds.
///
/// This streams instead. Ops go from the store to the file as the bytes
/// they are already stored as — never decoded, never re-encoded — through
/// a base64 encoder that works in 3-byte groups, into a temp file created
/// 0600, fsynced and renamed. Peak memory is the batch, not the log.
///
/// The format is unchanged: a JSON document whose `ops_b64` is the base64
/// of a postcard `Vec<OpEnvelope>`. A sequence in postcard is a varint
/// length followed by each element's encoding, and each element's encoding
/// is exactly what the store holds — which
/// `the_streamed_frame_matches_a_whole_encoding` pins down, because that
/// equivalence is the only reason this is allowed to skip the decode.
pub fn write_backup_streaming(
    out: &Path,
    data_dir: &Path,
    node_name: &str,
    genesis: &Genesis,
    engine: &StateEngine,
) -> Result<BackupReport> {
    use std::io::Write as _;

    let op_count = engine.op_count()?;
    let doc = header_doc(data_dir, node_name, genesis, op_count, OPS_SENTINEL)?;
    let json = serde_json::to_vec_pretty(&doc)
        .map_err(|e| CoreError::Store(format!("encode backup: {e}")))?;
    let at = find(&json, OPS_SENTINEL.as_bytes())
        .ok_or_else(|| CoreError::Store("backup header lost its ops placeholder".into()))?;
    let (head, rest) = json.split_at(at);
    let tail = &rest[OPS_SENTINEL.len()..];

    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("backup.tmp");
    let outcome = (|| -> Result<u64> {
        let file = open_private(&tmp)?;
        let mut w = std::io::BufWriter::with_capacity(1 << 16, file);
        w.write_all(head)?;
        let mut b64 = B64Writer::new(&mut w);
        // The sequence's length prefix, exactly as postcard writes one.
        b64.write(
            &postcard::to_stdvec(&(usize::try_from(op_count).unwrap_or(usize::MAX)))
                .map_err(|e| CoreError::Store(format!("encode op count: {e}")))?,
        )?;
        engine.for_each_op_bytes(|bytes| b64.write(bytes))?;
        b64.finish()?;
        w.write_all(tail)?;
        w.flush()?;
        let file = w
            .into_inner()
            .map_err(|e| CoreError::Store(format!("flush backup: {e}")))?;
        let size = file.metadata()?.len();
        // Durable before the rename: a backup whose bytes are still in the
        // page cache is not a backup.
        file.sync_all()?;
        Ok(size)
    })();
    let written = match outcome {
        Ok(size) => size,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, out)?;
    crate::durable::sync_dir(out.parent().unwrap_or(Path::new(".")));

    Ok(BackupReport {
        path: out.to_path_buf(),
        ops: op_count,
        bytes: written,
    })
}

/// The document with everything but the ops, which are streamed in place of
/// `ops_placeholder`.
fn header_doc(
    data_dir: &Path,
    node_name: &str,
    genesis: &Genesis,
    op_count: u64,
    ops_placeholder: &str,
) -> Result<BackupDoc> {
    let id_dir = Identity::identity_dir(data_dir);
    let node_key = std::fs::read(id_dir.join("node.key"))
        .map_err(|e| CoreError::Store(format!("cannot read node key: {e}")))?;
    let user_key = match std::fs::read(id_dir.join("user.key")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(CoreError::Store(format!("cannot read user key: {e}"))),
    };
    let user_key_kind = user_key_kind(&user_key)?;
    let genesis_json = serde_json::to_vec_pretty(genesis)
        .map_err(|e| CoreError::Store(format!("encode genesis: {e}")))?;
    Ok(BackupDoc {
        format: FORMAT,
        created_at_micros: mesh_proto::now_micros(),
        mesh_name: genesis.body.mesh_name.clone(),
        node_name: node_name.to_string(),
        genesis_b64: b64(&genesis_json),
        node_key_b64: b64(&node_key),
        user_key_b64: b64(&user_key),
        user_key_kind: user_key_kind.to_string(),
        ops_b64: ops_placeholder.to_string(),
        op_count,
        volumes_json_b64: b64(&read_volumes(data_dir)?),
    })
}

/// `volumes.json`, verbatim, or empty when this node has none (ENG-27).
///
/// Read as bytes rather than parsed and re-encoded: a backup should carry
/// what is on the disk, not this build's idea of it, so a field a later
/// release adds survives a restore by an earlier one.
fn read_volumes(data_dir: &Path) -> Result<Vec<u8>> {
    match std::fs::read(crate::volumes::store_path(data_dir)) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CoreError::Store(format!("cannot read volumes.json: {e}"))),
    }
}

/// Create a file 0600 *at creation*, not afterwards. The backup holds
/// private key seeds, and a create-then-chmod leaves a window in which they
/// are readable by the rest of the machine.
fn open_private(path: &Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Base64 straight into a writer, in 3-byte groups.
///
/// Base64 maps three input bytes to four output characters, so any prefix
/// whose length is a multiple of three encodes independently of what
/// follows. Everything else is held back until enough arrives — or until
/// [`Self::finish`], which encodes the remainder *with* padding, exactly
/// once, at the end.
struct B64Writer<'a, W: std::io::Write> {
    out: &'a mut W,
    /// At most two bytes: what did not fill a group.
    pending: Vec<u8>,
}

impl<'a, W: std::io::Write> B64Writer<'a, W> {
    fn new(out: &'a mut W) -> Self {
        B64Writer {
            out,
            pending: Vec::with_capacity(3),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut bytes = bytes;
        if !self.pending.is_empty() {
            let need = 3 - self.pending.len();
            let take = need.min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < 3 {
                return Ok(());
            }
            let group = std::mem::take(&mut self.pending);
            self.out
                .write_all(data_encoding::BASE64.encode(&group).as_bytes())?;
            self.pending = Vec::with_capacity(3);
        }
        let whole = bytes.len() - bytes.len() % 3;
        if whole > 0 {
            self.out
                .write_all(data_encoding::BASE64.encode(&bytes[..whole]).as_bytes())?;
        }
        self.pending.extend_from_slice(&bytes[whole..]);
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if !self.pending.is_empty() {
            self.out
                .write_all(data_encoding::BASE64.encode(&self.pending).as_bytes())?;
        }
        Ok(())
    }
}

/// Offline backup: open the data dir directly (the daemon must NOT be
/// running — the store is exclusively locked while it is).
pub fn offline_backup(data_dir: &Path, out: &Path) -> Result<BackupReport> {
    let genesis = Genesis::load(data_dir)?.ok_or_else(|| {
        CoreError::Store("no mesh here: the data dir has no genesis document".into())
    })?;
    let identity = Identity::load_or_generate(data_dir)?;
    let engine = StateEngine::open(data_dir, identity.node.clone(), genesis.clone())?;
    write_backup_streaming(out, data_dir, "offline", &genesis, &engine)
}

#[derive(Debug)]
pub struct RestoreReport {
    pub mesh_name: String,
    pub ops: u64,
    pub members: usize,
    /// Sinks held by the restore latch (M31 §19 Q5). Reported so the
    /// operator is told rather than discovering it when nothing exports.
    pub sinks_held: usize,
    /// The form the user key came back in: `plain`, `sealed`, `hardware`
    /// or `absent` (ENG-14).
    pub user_key: &'static str,
    /// Provisioned volumes restored from `volumes.json` (ENG-27). Zero for
    /// a node that had none, and for a backup taken before this was in it.
    pub volumes: usize,
}

/// Why a restored node waits before it writes (ENG-13), for whoever prints
/// a [`RestoreReport`].
pub const RESTORE_HOLD_NOTE: &str = "On its first start the node writes nothing of its own until it \
     has synced with a peer and picked up everything it wrote after this backup was taken; a node \
     with no other member starts writing at once.";

/// Read and validate a backup file's header (for `restore` preflight and
/// user confirmation output).
pub fn read_backup(path: &Path) -> Result<BackupDoc> {
    let bytes = std::fs::read(path)?;
    let doc: BackupDoc = serde_json::from_slice(&bytes)
        .map_err(|e| CoreError::Store(format!("not a meshware backup: {e}")))?;
    if doc.format > FORMAT {
        return Err(CoreError::Store(format!(
            "backup format {} is newer than this binary understands ({FORMAT})",
            doc.format
        )));
    }
    if doc.format == 0 {
        return Err(CoreError::Store(
            "backup format 0 is not a meshware backup".into(),
        ));
    }
    Ok(doc)
}

/// Put the backup's user key at `path`, minding whatever is there already.
///
/// A home with no mesh can still hold a person's key — Director mints one
/// on its first launch, the CLI on its first command, and a machine being
/// rebuilt may have a protected key its owner uses every day. The backup's
/// key is the one this mesh knows, so it normally goes in; the key already
/// there is set aside as `user.key.before-restore-<secs>`, never written
/// over, because a key is the one file nobody can make again.
///
/// **Except a protected key that may be the same person.** Restoring onto
/// the machine a backup came from commonly finds that identity sealed under
/// a passphrase since. Replacing it would drop protection somebody added on
/// purpose and strand a passphrase they still know, so a sealed key holding
/// the backup's identity is left exactly as it is — and so is one that is
/// locked, since without the passphrase "the same person" and "a stranger"
/// cannot be told apart, and the owner is one unlock away from settling it.
fn place_user_key(path: &Path, backup_key: &[u8]) -> Result<()> {
    use crate::keyfile::Stored;
    let Ok(existing) = std::fs::read(path) else {
        return crate::keyfile::write_private(path, backup_key);
    };
    if existing == backup_key {
        return Ok(());
    }
    if let (Ok(Stored::Plain(restored)), Ok(Stored::Sealed(sealed))) = (
        Stored::parse_bytes(backup_key),
        Stored::parse_bytes(&existing),
    ) {
        match crate::keyfile::cache_get(path).and_then(|kek| sealed.open_with_kek(&kek).ok()) {
            Some(seed) if seed == restored => return Ok(()),
            Some(_) => {}
            None => {
                tracing::warn!(
                    "{} is sealed and locked, so the restore left it alone. If the mesh \
                     now refuses you as an unknown user, that key is not the one in the backup",
                    path.display()
                );
                return Ok(());
            }
        }
    }
    let aside = path.with_extension(format!(
        "key.before-restore-{}",
        mesh_proto::now_micros() / 1_000_000
    ));
    std::fs::rename(path, &aside)?;
    tracing::info!(
        aside = %aside.display(),
        "restore: the key already here was set aside for the backup's"
    );
    crate::keyfile::write_private(path, backup_key)
}

/// Restore a backup into `data_dir` (offline; the target must not already
/// contain a mesh). Identity, genesis, and the op-log are rebuilt; state
/// materializes by replay, exactly as a sync would produce it.
pub fn restore(path: &Path, data_dir: &Path) -> Result<RestoreReport> {
    if Genesis::load(data_dir)?.is_some() {
        return Err(CoreError::Store(
            "the data dir already contains a mesh — remove it first to restore over it".into(),
        ));
    }
    let doc = read_backup(path)?;

    // A node key is always a bare seed: the daemon that uses it has nobody
    // to ask for a passphrase.
    let node_seed =
        match crate::keyfile::Stored::parse_bytes(&unb64(&doc.node_key_b64, "node_key")?)
            .map_err(|e| CoreError::Store(format!("backup node key is unusable: {e}")))?
        {
            crate::keyfile::Stored::Plain(seed) => seed,
            _ => {
                return Err(CoreError::Store(
                    "backup node key is not a plain key, and a node cannot unlock one".into(),
                ));
            }
        };
    // The user key goes back exactly as it was (ENG-14), once its bytes
    // are known to be a key file this build can use.
    let user_key_file = unb64(&doc.user_key_b64, "user_key")?;
    let user_key = user_key_kind(&user_key_file)
        .map_err(|e| CoreError::Store(format!("backup user key is unusable: {e}")))?;
    let genesis_json = unb64(&doc.genesis_b64, "genesis")?;
    let genesis: Genesis = serde_json::from_slice(&genesis_json)
        .map_err(|e| CoreError::Store(format!("backup genesis is corrupt: {e}")))?;
    genesis.verify()?;
    let ops_bytes = unb64(&doc.ops_b64, "ops")?;
    // A pre-F0 backup holds decoded ops; wrapping them re-derives exactly
    // the bytes their signatures already covered, so nothing is re-signed.
    let ops: Vec<OpEnvelope> = if doc.format >= 2 {
        postcard::from_bytes(&ops_bytes)
            .map_err(|e| CoreError::Store(format!("backup op-log is corrupt: {e}")))?
    } else {
        postcard::from_bytes::<Vec<Op>>(&ops_bytes)
            .map_err(|e| CoreError::Store(format!("backup op-log is corrupt: {e}")))?
            .iter()
            .map(OpEnvelope::from_op)
            .collect()
    };

    std::fs::create_dir_all(data_dir)?;
    let id_dir = Identity::identity_dir(data_dir);
    let node_key = Keypair::from_seed(node_seed);
    node_key.persist(&id_dir.join("node.key"))?;
    if !user_key_file.is_empty() {
        place_user_key(&id_dir.join("user.key"), &user_key_file)?;
    }
    genesis.save(data_dir)?;
    // This node's own note about the volumes it provisioned (ENG-27).
    // Written verbatim, before the engine opens, so a node that comes back
    // still claims the directories it made — they are on the disk either
    // way, and the record is the only thing that connects them to the
    // mesh. Absent in a backup taken before the field existed, which
    // restores exactly as it used to.
    let volumes = unb64(&doc.volumes_json_b64, "volumes")?;
    let volumes_restored = if volumes.is_empty() {
        0
    } else {
        std::fs::write(crate::volumes::store_path(data_dir), &volumes)?;
        crate::volumes::Store::load(data_dir).volumes.len()
    };

    let engine = StateEngine::open(data_dir, node_key, genesis.clone())?;
    let count = ops.len() as u64;
    engine.ingest(ops)?;
    engine.flush()?;
    let members = engine.read(|s| s.members.len());
    // Held until a peer has shown it its own history (ENG-13). Its peers
    // may hold everything it wrote after the backup was taken, under
    // sequence numbers it would otherwise hand out again — and they drop a
    // duplicate for good. After the ingest, which is this node's own log
    // coming back and is read as such.
    crate::state::RestoreHold::place(data_dir, "restore")?;

    // M31 §19 Q5, decided: **a restore holds every sink until an admin
    // looks at the list.** A restored mesh must not silently re-push years
    // of history into a live warehouse, and a *forked* one — a restore that
    // mints a fresh identity — must not push into a warehouse belonging to
    // the mesh it forked from, with that mesh's credentials.
    //
    // Node-local rather than a flag in the replicated record: see
    // `sinks::pause_after_restore` for why that is the safer shape.
    let sinks = crate::layout::Layout::new(data_dir).sinks_dir();
    let sinks_held = engine.read(|s| s.sinks.values().filter(|e| !e.removed).count());
    if sinks_held > 0 {
        crate::sinks::pause_after_restore(&sinks)?;
    }

    Ok(RestoreReport {
        mesh_name: genesis.body.mesh_name,
        ops: count,
        members,
        sinks_held,
        user_key,
        volumes: volumes_restored,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// **The equivalence the streaming path rests on** (ENG-20).
    ///
    /// Writing a backup without decoding the log is only correct if a
    /// postcard sequence is a varint length followed by each element's own
    /// encoding — the bytes the store already holds. If postcard ever
    /// framed a sequence differently, every backup taken would be
    /// unreadable and nothing would notice until a restore. So it is
    /// asserted, on the real type, rather than assumed from the spec.
    #[test]
    fn the_streamed_frame_matches_a_whole_encoding() {
        let env = |seq: u64| OpEnvelope {
            author: mesh_proto::NodeId([seq as u8; 32]),
            seq,
            hlc: mesh_proto::Hlc {
                wall_micros: 1_000 + seq,
                logical: 0,
            },
            body: vec![1, 2, 3, seq as u8],
            signature: vec![9u8; 64],
        };
        for count in [0usize, 1, 2, 127, 128, 300] {
            let ops: Vec<OpEnvelope> = (0..count as u64).map(env).collect();
            let whole = postcard::to_stdvec(&ops).unwrap();

            let mut streamed = postcard::to_stdvec(&count).unwrap();
            for op in &ops {
                streamed.extend_from_slice(&postcard::to_stdvec(op).unwrap());
            }
            assert_eq!(streamed, whole, "count {count}");
        }
    }

    /// The base64 writer must produce exactly what encoding the whole input
    /// at once produces, whatever sizes the pieces arrive in — including
    /// pieces that do not land on a 3-byte boundary, which is every real
    /// op.
    #[test]
    fn the_streaming_base64_matches_encoding_it_all_at_once() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for chunk in [1usize, 2, 3, 4, 5, 7, 64, 999, 1000] {
            let mut out: Vec<u8> = Vec::new();
            {
                let mut w = B64Writer::new(&mut out);
                for piece in data.chunks(chunk) {
                    w.write(piece).unwrap();
                }
                w.finish().unwrap();
            }
            assert_eq!(
                String::from_utf8(out).unwrap(),
                data_encoding::BASE64.encode(&data),
                "chunk size {chunk}"
            );
        }
        // …and nothing at all encodes to nothing, not to padding.
        let mut empty: Vec<u8> = Vec::new();
        B64Writer::new(&mut empty).finish().unwrap();
        assert!(empty.is_empty());
    }

    /// The streaming writer and the one it replaced must produce the same
    /// document for the same log. Anything else is a backup format that
    /// changed without anybody deciding to change it.
    #[test]
    fn a_streamed_backup_is_the_same_document_as_a_buffered_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let founder = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        node.persist(&Identity::identity_dir(&home).join("node.key"))
            .unwrap();
        let genesis = Genesis::create("eng20", &founder, node.node_id()).unwrap();
        genesis.save(&home).unwrap();
        let engine = StateEngine::open(&home, node.clone(), genesis.clone()).unwrap();
        // Enough ops that the base64 stream crosses many 3-byte groups and
        // the varint length prefix needs two bytes.
        for i in 0..200u8 {
            engine
                .author_op(mesh_proto::OpBody::AuditAppend {
                    entry: mesh_proto::AuditEntry {
                        audit_id: mesh_proto::AuditId([i; 16]),
                        at_micros: u64::from(i),
                        user: founder.user_id(),
                        username: "founder".into(),
                        action: "job.run".into(),
                        target: format!("target-{i}"),
                        node: node.node_id(),
                        outcome: "ok".into(),
                        detail: String::new(),
                        request_sig: vec![],
                    },
                })
                .unwrap();
        }

        let streamed_path = dir.path().join("streamed.mwb");
        let buffered_path = dir.path().join("buffered.mwb");
        let streamed =
            write_backup_streaming(&streamed_path, &home, "n1", &genesis, &engine).unwrap();
        let buffered = write_backup(
            &buffered_path,
            &home,
            "n1",
            &genesis,
            engine.all_ops().unwrap(),
        )
        .unwrap();

        assert_eq!(streamed.ops, buffered.ops);
        assert!(streamed.ops >= 200);
        let read =
            |p: &Path| -> BackupDoc { serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap() };
        let (a, b) = (read(&streamed_path), read(&buffered_path));
        assert_eq!(a.ops_b64, b.ops_b64, "the op payload must be identical");
        assert_eq!(a.op_count, b.op_count);
        assert_eq!(a.genesis_b64, b.genesis_b64);
        assert_eq!(a.node_key_b64, b.node_key_b64);

        // …and it decodes back to the ops that went in. Compared by their
        // encodings, which is the equality that matters here and the one
        // `OpEnvelope` does not implement.
        let ops: Vec<OpEnvelope> =
            postcard::from_bytes(&unb64(&a.ops_b64, "ops").unwrap()).unwrap();
        let encode = |v: &[OpEnvelope]| -> Vec<Vec<u8>> {
            v.iter().map(|o| postcard::to_stdvec(o).unwrap()).collect()
        };
        assert_eq!(encode(&ops), encode(&engine.all_ops().unwrap()));
    }

    /// **Both** writers create the file 0600, not chmod it afterwards
    /// (ENG-27).
    ///
    /// The streaming one always did; the buffered one wrote the file and
    /// then changed its mode, leaving a window in which a document holding
    /// this node's private key seed *and its user's* was readable by
    /// everything else on the machine. A test that checked only one of two
    /// writers of the same secrets is how that survived.
    #[test]
    fn every_backup_writer_creates_the_file_private() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let founder = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        node.persist(&Identity::identity_dir(&home).join("node.key"))
            .unwrap();
        let genesis = Genesis::create("eng27", &founder, node.node_id()).unwrap();
        genesis.save(&home).unwrap();
        let engine = StateEngine::open(&home, node.clone(), genesis.clone()).unwrap();

        let streamed = dir.path().join("streamed.mwb");
        write_backup_streaming(&streamed, &home, "n1", &genesis, &engine).unwrap();
        let buffered = dir.path().join("buffered.mwb");
        write_backup(&buffered, &home, "n1", &genesis, engine.all_ops().unwrap()).unwrap();

        #[cfg(unix)]
        for path in [&streamed, &buffered] {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "{} is {mode:o}",
                path.file_name().unwrap().to_string_lossy()
            );
        }
    }

    /// `volumes.json` is the one thing in the data directory no peer can
    /// give back (ENG-27): the op-log is replicated and the content store
    /// is re-fetchable, but a volume is a directory on *this* machine, so
    /// its record is deliberately node-local. Leaving it out of the backup
    /// was a silent data loss — restore the node and its provisioned
    /// volumes are gone, with the directories still on the disk and
    /// nothing claiming them.
    #[test]
    fn provisioned_volumes_survive_a_backup_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let founder = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        node.persist(&Identity::identity_dir(&home).join("node.key"))
            .unwrap();
        let genesis = Genesis::create("eng27-vol", &founder, node.node_id()).unwrap();
        genesis.save(&home).unwrap();
        let engine = StateEngine::open(&home, node.clone(), genesis.clone()).unwrap();

        let mut volumes = crate::volumes::Store::default();
        volumes.volumes.push(crate::volumes::Created {
            name: "scratch".into(),
            path: "/srv/scratch".into(),
            read_only: false,
            plugin: "lvm".into(),
            capacity_mb: 4096,
            detail: "vg0/scratch".into(),
            created_at_micros: mesh_proto::now_micros(),
            params: Default::default(),
        });
        volumes.save(&home).unwrap();

        let out = dir.path().join("node.mwb");
        write_backup(&out, &home, "n1", &genesis, engine.all_ops().unwrap()).unwrap();
        drop(engine);

        let restored_home = dir.path().join("restored");
        let report = restore(&out, &restored_home).unwrap();
        assert_eq!(report.volumes, 1, "the volume did not come back");
        let back = crate::volumes::Store::load(&restored_home);
        assert_eq!(back.volumes.len(), 1);
        assert_eq!(back.volumes[0].name, "scratch");
        assert_eq!(back.volumes[0].path, "/srv/scratch");
    }

    /// The file holds private key seeds, so it is 0600 from the moment it
    /// exists — and it appears at its final name only once it is complete.
    #[test]
    fn a_streamed_backup_is_private_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let founder = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        node.persist(&Identity::identity_dir(&home).join("node.key"))
            .unwrap();
        let genesis = Genesis::create("eng20", &founder, node.node_id()).unwrap();
        genesis.save(&home).unwrap();
        let engine = StateEngine::open(&home, node.clone(), genesis.clone()).unwrap();

        let out = dir.path().join("node.mwb");
        write_backup_streaming(&out, &home, "n1", &genesis, &engine).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "left a temp file behind: {stray:?}");
    }

    /// A node's home with the given user key file, and a backup of it.
    fn backed_up(dir: &Path, user_key_file: Option<&[u8]>) -> PathBuf {
        let home = dir.join("home");
        let founder = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        let id_dir = Identity::identity_dir(&home);
        node.persist(&id_dir.join("node.key")).unwrap();
        if let Some(bytes) = user_key_file {
            crate::keyfile::write_private(&id_dir.join("user.key"), bytes).unwrap();
        }
        let genesis = Genesis::create("eng14", &founder, node.node_id()).unwrap();
        genesis.save(&home).unwrap();
        let out = dir.join("node.mwb");
        write_backup(&out, &home, "n1", &genesis, Vec::new()).unwrap();
        out
    }

    fn restored_user_key(dir: &Path, backup: &Path) -> (Option<Vec<u8>>, &'static str) {
        let target = dir.join("restored");
        let report = restore(backup, &target).unwrap();
        let file = Identity::identity_dir(&target).join("user.key");
        (std::fs::read(file).ok(), report.user_key)
    }

    /// Every form of user key comes back byte for byte (ENG-14). Only the
    /// plain one used to: a sealed or hardware key file was refused for not
    /// being 32 bytes.
    #[test]
    fn every_kind_of_user_key_survives_a_restore() {
        let seed = [7u8; 32];
        let sealed = crate::keyfile::seal(&seed, b"correct horse").unwrap();
        let hardware = crate::keyfile::hardware_ref(&[9u8; 32]);
        for (bytes, kind) in [
            (seed.to_vec(), "plain"),
            (sealed, "sealed"),
            (hardware, "hardware"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let backup = backed_up(dir.path(), Some(&bytes));
            assert_eq!(read_backup(&backup).unwrap().user_key_kind, kind);
            let (back, reported) = restored_user_key(dir.path(), &backup);
            assert_eq!(back.as_deref(), Some(bytes.as_slice()), "{kind}");
            assert_eq!(reported, kind);
        }
    }

    /// The sealed key that comes back still opens with its passphrase.
    #[test]
    fn a_restored_sealed_key_opens_with_its_passphrase() {
        let seed = [3u8; 32];
        let sealed = crate::keyfile::seal(&seed, b"hunter2").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), Some(&sealed));
        let (back, _) = restored_user_key(dir.path(), &backup);
        let crate::keyfile::Stored::Sealed(file) =
            crate::keyfile::Stored::parse_bytes(&back.unwrap()).unwrap()
        else {
            panic!("not sealed");
        };
        assert_eq!(file.open(b"hunter2").unwrap(), seed);
        assert!(file.open(b"wrong").is_err());
    }

    /// Restoring into a home that already holds somebody's key — the one
    /// Director mints on first launch, or a protected key in daily use —
    /// sets that key aside instead of writing over it.
    #[test]
    fn a_key_already_in_the_home_is_set_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), Some(&[1u8; 32]));
        let target = dir.path().join("restored");
        let id_dir = Identity::identity_dir(&target);
        crate::keyfile::write_private(&id_dir.join("user.key"), &[2u8; 32]).unwrap();

        restore(&backup, &target).unwrap();
        assert_eq!(std::fs::read(id_dir.join("user.key")).unwrap(), [1u8; 32]);
        let aside: Vec<_> = std::fs::read_dir(&id_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("user.key.before-restore-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(aside[0].path()).unwrap(), [2u8; 32]);
    }

    /// The backup's identity, sealed since: the seal stays.
    #[test]
    fn a_sealed_key_holding_the_backups_identity_keeps_its_seal() {
        let seed = [7u8; 32];
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), Some(&seed));
        let target = dir.path().join("restored");
        let key = Identity::identity_dir(&target).join("user.key");
        let sealed = crate::keyfile::seal(&seed, b"hunter2").unwrap();
        crate::keyfile::write_private(&key, &sealed).unwrap();
        // Prime the session unlock, so this takes the *recognised the
        // identity* branch rather than the "cannot tell" one — which would
        // pass for the wrong reason. Best-effort: with no
        // `$XDG_RUNTIME_DIR` there is no cache, and the answer is the same.
        if let Ok(crate::keyfile::Stored::Sealed(parsed)) = crate::keyfile::Stored::read(&key)
            && let Ok(kek) = parsed.kek(b"hunter2")
        {
            let _ = crate::keyfile::cache_put(&key, &kek);
        }

        restore(&backup, &target).unwrap();
        assert_eq!(
            std::fs::read(&key).unwrap(),
            sealed,
            "a sealed key keeps its seal"
        );
        crate::keyfile::cache_clear(&key);
    }

    /// Sealed, locked, and perhaps somebody else: left alone.
    #[test]
    fn a_sealed_key_that_cannot_be_opened_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), Some(&[7u8; 32]));
        let target = dir.path().join("restored");
        let key = Identity::identity_dir(&target).join("user.key");
        let sealed = crate::keyfile::seal(&[9u8; 32], b"hunter2").unwrap();
        crate::keyfile::write_private(&key, &sealed).unwrap();

        restore(&backup, &target).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), sealed);
    }

    #[test]
    fn a_node_without_a_user_key_backs_up_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), None);
        let (back, reported) = restored_user_key(dir.path(), &backup);
        assert_eq!(back, None);
        assert_eq!(reported, "absent");
    }

    /// A backup written before the kind was recorded is read by its bytes.
    #[test]
    fn an_older_backup_without_a_kind_still_restores() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = crate::keyfile::seal(&[5u8; 32], b"pw").unwrap();
        let backup = backed_up(dir.path(), Some(&sealed));
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&backup).unwrap()).unwrap();
        doc.as_object_mut().unwrap().remove("user_key_kind");
        std::fs::write(&backup, serde_json::to_vec(&doc).unwrap()).unwrap();
        let (back, reported) = restored_user_key(dir.path(), &backup);
        assert_eq!(back.as_deref(), Some(sealed.as_slice()));
        assert_eq!(reported, "sealed");
    }

    #[test]
    fn a_user_key_that_is_not_a_key_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let backup = backed_up(dir.path(), Some(&[1u8; 32]));
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&backup).unwrap()).unwrap();
        doc["user_key_b64"] = serde_json::Value::String(b64(b"not a key"));
        std::fs::write(&backup, serde_json::to_vec(&doc).unwrap()).unwrap();
        let err = restore(&backup, &dir.path().join("restored")).unwrap_err();
        assert!(err.to_string().contains("user key is unusable"), "{err}");
    }
}
