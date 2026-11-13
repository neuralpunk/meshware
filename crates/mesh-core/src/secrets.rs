//! Secrets: envelope encryption, late-binding resolution, provider plugins,
//! and log redaction (MASTERPLAN §7.3).
//!
//! Sealing happens *client-side* (CLI/Director): the value is encrypted with
//! XChaCha20-Poly1305 under a random data key, and the data key is wrapped
//! per node via an X25519 sealed box derived from each node's Ed25519 key.
//! The plaintext never transits the daemon or replicated state; it exists
//! only in the sealing client and, briefly, in the executing process.
//!
//! References — `{{secret:name}}` and `{{secret_file:name}}` — are legal in
//! environment values only (never argv: `/proc/<pid>/cmdline` is public) and
//! resolve on the executing node at spawn time. Provider references take the
//! form `{{secret:provider:path#key}}` and shell out to a configured plugin
//! using the adopted contract: `plugin get`, JSON `{path,key,config}` on
//! stdin, `{"value":…}` on stdout, non-zero exit = failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use curve25519_dalek::MontgomeryPoint;
use mesh_proto::{NodeId, SecretRecord, SecretWrap, UserId};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::config::ProviderConfig;
use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::state::{MeshState, StateEngine};

/// blake3 KDF context for wrap keys. Changing this is a protocol break.
const WRAP_CONTEXT: &str = "meshware secret wrap v1";
/// Secrets shorter than this are not registered for redaction (too many
/// false positives) — and are a bad idea anyway.
const REDACT_MIN_LEN: usize = 4;
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Envelope crypto
// ---------------------------------------------------------------------------

fn aead(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(*key))
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf)
        .map_err(|e| CoreError::Secret(format!("system rng unavailable: {e}")))?;
    Ok(buf)
}

/// X25519 public key of a node, derived from its Ed25519 identity.
fn node_montgomery(node: &NodeId) -> Result<MontgomeryPoint> {
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&node.0)
        .map_err(|e| CoreError::Secret(format!("node key {} invalid: {e}", node.short())))?;
    Ok(vk.to_montgomery())
}

fn wrap_key_for(shared: &[u8; 32], eph_pub: &[u8; 32], recipient: &NodeId) -> [u8; 32] {
    let mut material = Vec::with_capacity(96);
    material.extend_from_slice(shared);
    material.extend_from_slice(eph_pub);
    material.extend_from_slice(&recipient.0);
    blake3::derive_key(WRAP_CONTEXT, &material)
}

/// Wrap a data key to one node: ephemeral X25519 ECDH + XChaCha20-Poly1305.
fn wrap_for(data_key: &[u8; 32], node: &NodeId) -> Result<SecretWrap> {
    let recipient = node_montgomery(node)?;
    let eph: Zeroizing<[u8; 32]> = Zeroizing::new(random()?);
    let eph_pub = MontgomeryPoint::mul_base_clamped(*eph).to_bytes();
    let shared = recipient.mul_clamped(*eph).to_bytes();
    if shared == [0u8; 32] {
        return Err(CoreError::Secret(format!(
            "degenerate shared secret for node {}",
            node.short()
        )));
    }
    let key = Zeroizing::new(wrap_key_for(&shared, &eph_pub, node));
    let nonce: [u8; 24] = random()?;
    let wrapped_key = aead(&key)
        .encrypt(&XNonce::from(nonce), data_key.as_slice())
        .map_err(|_| CoreError::Secret("wrap encryption failed".into()))?;
    Ok(SecretWrap {
        node_id: *node,
        eph_pub,
        nonce,
        wrapped_key,
    })
}

/// Wrap a data key for many nodes.
pub fn make_wraps(data_key: &[u8; 32], recipients: &[NodeId]) -> Result<Vec<SecretWrap>> {
    recipients.iter().map(|n| wrap_for(data_key, n)).collect()
}

/// Seal a secret value for a set of recipient nodes. Returns an *unsigned*
/// record — the caller signs it with the creating user's key.
pub fn seal_secret(
    name: &str,
    version: u32,
    value: &[u8],
    recipients: &[NodeId],
    created_by: UserId,
) -> Result<SecretRecord> {
    if recipients.is_empty() {
        return Err(CoreError::Secret(
            "no recipient nodes — nothing could ever decrypt this secret".into(),
        ));
    }
    let data_key: Zeroizing<[u8; 32]> = Zeroizing::new(random()?);
    let nonce: [u8; 24] = random()?;
    let ciphertext = aead(&data_key)
        .encrypt(&XNonce::from(nonce), value)
        .map_err(|_| CoreError::Secret("value encryption failed".into()))?;
    let wraps = make_wraps(&data_key, recipients)?;
    Ok(SecretRecord {
        name: name.to_string(),
        version,
        nonce,
        ciphertext,
        wraps,
        created_by,
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    })
}

/// Unwrap the data key with this node's identity key.
///
/// **Every wrap addressed to this node is tried, and a key only counts if
/// it opens the value** (SEC-17). Wraps are not covered by the admin's
/// signature — they cannot be, since nodes extend them for late joiners —
/// so any member can add one addressed to anybody. Taking the first one
/// that matched the node id let a member plant an undecryptable wrap for a
/// node that had just joined and lock it out of the secret. The AEAD tag on
/// the value is the proof: a key that opens it is the key.
pub fn unwrap_data_key(record: &SecretRecord, node_key: &Keypair) -> Result<Zeroizing<[u8; 32]>> {
    let me = node_key.node_id();
    let scalar = Zeroizing::new(node_key.to_x25519_scalar_bytes());
    let mut addressed = false;
    for wrap in record.wraps.iter().filter(|w| w.node_id == me) {
        addressed = true;
        let shared = MontgomeryPoint(wrap.eph_pub)
            .mul_clamped(*scalar)
            .to_bytes();
        let key = Zeroizing::new(wrap_key_for(&shared, &wrap.eph_pub, &me));
        let Ok(opened) = aead(&key).decrypt(&XNonce::from(wrap.nonce), wrap.wrapped_key.as_slice())
        else {
            continue;
        };
        let opened = Zeroizing::new(opened);
        let Ok(dk) = <[u8; 32]>::try_from(opened.as_slice()) else {
            continue;
        };
        let dk = Zeroizing::new(dk);
        if aead(&dk)
            .decrypt(&XNonce::from(record.nonce), record.ciphertext.as_slice())
            .map(Zeroizing::new)
            .is_ok()
        {
            return Ok(dk);
        }
    }
    Err(CoreError::Secret(if addressed {
        format!(
            "no wrap of secret {:?} for this node opens it — an authorized holder will rewrap \
             it shortly, or re-set the secret",
            record.name
        )
    } else {
        format!(
            "secret {:?} is not wrapped for this node — an authorized holder will rewrap \
             it shortly, or re-set the secret",
            record.name
        )
    }))
}

/// Decrypt a secret's value with this node's identity key.
pub fn open_secret(record: &SecretRecord, node_key: &Keypair) -> Result<Zeroizing<Vec<u8>>> {
    let data_key = unwrap_data_key(record, node_key)?;
    let value = aead(&data_key)
        .decrypt(&XNonce::from(record.nonce), record.ciphertext.as_slice())
        .map_err(|_| CoreError::Secret(format!("cannot decrypt secret {:?}", record.name)))?;
    Ok(Zeroizing::new(value))
}

/// Produce wraps for `missing` nodes, given a node that can already unwrap.
/// Backs the rewrap path that heals secrets for late joiners.
pub fn rewrap_for(
    record: &SecretRecord,
    node_key: &Keypair,
    missing: &[NodeId],
) -> Result<Vec<SecretWrap>> {
    let data_key = unwrap_data_key(record, node_key)?;
    make_wraps(&data_key, missing)
}

/// The marker that makes a `SecretRewrap` its author's whole statement
/// (SEC-17): a "wrap" addressed to the author itself, with no key in it.
/// A node that predates it sees a wrap for a node that already holds one
/// and skips it, so the marker costs an older node nothing.
const STATEMENT_MAGIC: &[u8; 8] = b"mw-heal1";

/// The marker entry for `author`, asking to be healed `wants` times.
pub fn statement_marker(author: NodeId, wants: u32) -> SecretWrap {
    let mut nonce = [0u8; 24];
    nonce[..8].copy_from_slice(STATEMENT_MAGIC);
    nonce[8..12].copy_from_slice(&wants.to_le_bytes());
    SecretWrap {
        node_id: author,
        eph_pub: [0u8; 32],
        nonce,
        wrapped_key: Vec::new(),
    }
}

/// Does this rewrap restate everything its author provides, or is it an
/// older node's plain "here are the ones that were missing" (ENG-18)?
///
/// The marker is what tells the two apart, and the difference decides
/// whether a later rewrap may *supersede* an earlier one. A total
/// statement may: it says everything, so the earlier one is dead weight.
/// An additive one may not — each carries only the recipients that were
/// missing when it was written, so dropping an earlier one drops those
/// recipients' wraps for good, and a node rebuilt from what is left cannot
/// read the secret.
///
/// Author-free on purpose: only a node writes its own marker, so the
/// marker's presence identifies the form without the envelope, and
/// `compact::compaction` classifies a body on its own.
pub fn is_total_statement(wraps: &[SecretWrap]) -> bool {
    wraps
        .iter()
        .any(|w| w.wrapped_key.is_empty() && w.nonce[..8] == STATEMENT_MAGIC[..])
}

/// If `wraps` is a statement by `author`, how many times it is asking to
/// be healed; `None` for an older node's plain rewrap.
pub fn read_statement(author: &NodeId, wraps: &[SecretWrap]) -> Option<u32> {
    wraps
        .iter()
        .find(|w| {
            w.node_id == *author && w.wrapped_key.is_empty() && w.nonce[..8] == STATEMENT_MAGIC[..]
        })
        .map(|w| {
            let mut n = [0u8; 4];
            n.copy_from_slice(&w.nonce[8..12]);
            u32::from_le_bytes(n)
        })
}

/// Should `node` hold the secret `name` (SEC-7)?
///
/// Never a node that is not an active member, or is quarantined. Past
/// that, a scoped secret goes to the nodes its selector matches, and an
/// unscoped one to every member except no-execute nodes: a machine that
/// never runs work has nothing to use a secret for.
pub fn is_recipient(state: &MeshState, name: &str, node: &mesh_proto::MemberInfo) -> bool {
    let nodes = state.secret_scopes.get(name).map(|s| s.nodes.as_str());
    is_recipient_for(state, nodes, node)
}

/// [`is_recipient`], for a scope's node selector given directly — one
/// about to be set.
pub fn is_recipient_for(
    state: &MeshState,
    nodes: Option<&str>,
    node: &mesh_proto::MemberInfo,
) -> bool {
    if !state.is_active_member(&node.node_id) || state.is_quarantined(&node.node_id) {
        return false;
    }
    match nodes.map(str::trim).filter(|n| !n.is_empty()) {
        Some(selector) => crate::membership::Selector::parse(selector).matches_in(state, node),
        None => !node.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG),
    }
}

/// May `user` use the secret `name` in work running on `node` (SEC-7)?
/// `user` is `None` for the node's own use — a sink credential an admin
/// configured — which the node half still applies to.
pub fn may_use(
    state: &MeshState,
    founder: &[u8; 32],
    name: &str,
    user: Option<&UserId>,
    node: &NodeId,
    now_micros: u64,
) -> std::result::Result<(), String> {
    let Some(member) = state.members.get(node) else {
        return Err("this node is not a member".into());
    };
    if !is_recipient(state, name, &member.info) {
        return Err(format!(
            "secret {name:?} is not for this node — its scope, or this node's tags or \
             quarantine, leave it out"
        ));
    }
    let Some(scope) = state.secret_scopes.get(name).filter(|s| s.limits_users()) else {
        return Ok(());
    };
    let Some(user) = user else {
        return Ok(());
    };
    let named = scope.users.contains(user) || user.0 == *founder;
    let by_role = crate::rbac::roles_at(state, founder, user, now_micros)
        .iter()
        .any(|r| scope.roles.contains(r));
    if named || by_role {
        Ok(())
    } else {
        Err(format!(
            "{} may not use secret {name:?} — its scope names who may",
            crate::rbac::username_of(state, founder, user)
        ))
    }
}

/// How many nodes heal a late joiner at first (SEC-17). Each widening round
/// — the joiner saying it still cannot open the secret — doubles it.
pub const SECRET_HEALERS: usize = 3;

/// Whether `me` is one of the nodes that should heal `target` for this
/// secret, out of `candidates` (the live members other than the target),
/// after `round` requests from it.
///
/// Every holder used to heal every joiner, which was a wrap per holder per
/// joiner once wraps stopped being first-come-first-served. A rendezvous
/// rank keeps it to a few, and different ones per joiner; the rounds are
/// what finds a holder when the first few ranked cannot open it
/// themselves.
pub fn is_healer(
    record: &SecretRecord,
    target: &NodeId,
    me: &NodeId,
    candidates: impl IntoIterator<Item = NodeId>,
    round: u32,
) -> bool {
    let mut key = Vec::with_capacity(record.name.len() + 4 + 32);
    key.extend_from_slice(record.name.as_bytes());
    key.extend_from_slice(&record.version.to_le_bytes());
    key.extend_from_slice(&target.0);
    let width = SECRET_HEALERS.saturating_mul(1usize << round.min(16));
    crate::scheduler::rendezvous_rank(b"mw-secret-heal-v1", &key, candidates)
        .into_iter()
        .take(width)
        .any(|n| n == *me)
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// Where a reference's value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    /// The mesh vault: `{{secret:name}}`.
    Vault { name: String },
    /// An external provider plugin: `{{secret:provider:path#key}}`.
    Provider {
        provider: String,
        path: String,
        key: String,
    },
}

impl SecretSource {
    /// Stable label for audit rows and redaction markers.
    pub fn label(&self) -> String {
        match self {
            SecretSource::Vault { name } => name.clone(),
            SecretSource::Provider {
                provider,
                path,
                key,
            } => format!("{provider}:{path}#{key}"),
        }
    }
}

/// One `{{secret:…}}` / `{{secret_file:…}}` occurrence inside a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretUse {
    /// The verbatim reference text, for substitution.
    pub raw: String,
    /// File injection (`{{secret_file:…}}`) vs inline env value.
    pub file: bool,
    pub source: SecretSource,
}

/// Find all secret references in a string. Malformed references (unclosed,
/// empty name) are left untouched — they are not secrets.
pub fn find_refs(value: &str) -> Vec<SecretUse> {
    let mut out = Vec::new();
    for (marker, file) in [("{{secret:", false), ("{{secret_file:", true)] {
        let mut rest = value;
        while let Some(start) = rest.find(marker) {
            let after = &rest[start + marker.len()..];
            let Some(end) = after.find("}}") else { break };
            let inner = &after[..end];
            rest = &after[end + 2..];
            if inner.is_empty() || inner.contains('{') {
                continue;
            }
            let source = match inner.split_once(':') {
                None => SecretSource::Vault {
                    name: inner.to_string(),
                },
                Some((provider, path_key)) => {
                    let (path, key) = path_key.split_once('#').unwrap_or((path_key, ""));
                    SecretSource::Provider {
                        provider: provider.to_string(),
                        path: path.to_string(),
                        key: key.to_string(),
                    }
                }
            };
            let u = SecretUse {
                raw: format!("{marker}{inner}}}}}"),
                file,
                source,
            };
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    // `{{secret_file:` also contains `{{secret` — but the marker scan for
    // `{{secret:` requires the colon right after, so the two never overlap.
    out
}

/// The outcome of resolving an environment's secret references.
#[derive(Debug, Default)]
pub struct ResolvedEnv {
    pub env: Vec<(String, String)>,
    /// (label, value bytes) registered with the log redaction filter.
    pub redactions: Vec<(String, Vec<u8>)>,
    /// Labels of every reference resolved, for the audit trail.
    pub resolved: Vec<String>,
}

/// Resolve every secret reference in `env` on this (executing) node.
/// File-mode references are written under `secret_dir` (created 0700, files
/// 0400) and replaced by the file path; inline references are replaced by
/// the value. Fails closed: any unresolvable reference fails the job.
///
/// `user` is whose work this is: a vault secret is only resolved for a
/// user its scope allows, on a node it is for (SEC-7). `None` is the
/// node's own use (a sink credential), where the node half still applies.
pub async fn resolve_env(
    engine: &Arc<StateEngine>,
    node_key: &Keypair,
    providers: &BTreeMap<String, ProviderConfig>,
    env: Vec<(String, String)>,
    secret_dir: &Path,
    user: Option<&UserId>,
) -> Result<ResolvedEnv> {
    let mut out = ResolvedEnv::default();
    let mut file_index = 0usize;
    for (k, v) in env {
        let refs = find_refs(&v);
        if refs.is_empty() {
            out.env.push((k, v));
            continue;
        }
        let mut value = v;
        for r in refs {
            let plain: Zeroizing<Vec<u8>> = match &r.source {
                SecretSource::Vault { name } => {
                    let founder = engine.genesis().body.founder_user;
                    let me = node_key.node_id();
                    let now = mesh_proto::now_micros();
                    let record = engine
                        .read(|s| {
                            may_use(s, &founder, name, user, &me, now)?;
                            s.secrets
                                .get(name)
                                .filter(|e| !e.removed)
                                .map(|e| e.record.clone())
                                .ok_or_else(|| {
                                    format!("no secret named {name:?} in the mesh vault")
                                })
                        })
                        .map_err(CoreError::Secret)?;
                    open_secret(&record, node_key)?
                }
                SecretSource::Provider {
                    provider,
                    path,
                    key,
                } => {
                    let cfg = providers.get(provider).ok_or_else(|| {
                        CoreError::Secret(format!(
                            "no secret provider {provider:?} configured on this node"
                        ))
                    })?;
                    Zeroizing::new(
                        provider_get(&cfg.plugin, path, key, &cfg.config)
                            .await?
                            .into_bytes(),
                    )
                }
            };
            let label = r.source.label();
            let replacement = if r.file {
                let path = write_secret_file(secret_dir, file_index, &plain)?;
                file_index += 1;
                path.to_string_lossy().into_owned()
            } else {
                String::from_utf8(plain.to_vec()).map_err(|_| {
                    CoreError::Secret(format!(
                        "secret {label:?} is not valid UTF-8 — inject it with {{{{secret_file:…}}}} instead"
                    ))
                })?
            };
            if plain.len() >= REDACT_MIN_LEN {
                out.redactions.push((label.clone(), plain.to_vec()));
            }
            out.resolved.push(label);
            value = value.replace(&r.raw, &replacement);
        }
        out.env.push((k, value));
    }
    Ok(out)
}

/// Write one secret value as a mode-0400 file in the job's private secret
/// dir. The directory is created 0700 on first use.
fn write_secret_file(dir: &Path, index: usize, value: &[u8]) -> Result<PathBuf> {
    // Modes are unix-only; non-unix builds exist solely for Director's
    // non-executing peers (D6/D10), which never write secret files.
    //
    // **Created with the mode, not created and then given it** (S-L1). Both
    // halves used to exist for a moment at `& ~umask` before being narrowed
    // — a directory of secrets at 0755 and a secret at 0644 — which is a
    // window a local user on a shared machine can wait for.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // `mode` is only applied to directories this call created; an
        // existing one keeps what it had.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!("s{index}"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            // 0400 on a file this process is about to write: the mode
            // governs later opens, never the descriptor already in hand.
            .mode(0o400)
            .open(&path)?;
        file.write_all(value)?;
        file.sync_all()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
        Ok(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("s{index}"));
        std::fs::write(&path, value)?;
        Ok(path)
    }
}

/// Best-effort destruction of injected secret files after a job ends:
/// overwrite with zeros, then remove the directory.
pub fn shred_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            let len = usize::try_from(meta.len()).unwrap_or(0);
            let _ = std::fs::write(&path, vec![0u8; len]);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Provider plugins (contract adopted from the competitor teardown — their
// best design: subprocess, JSON stdin/stdout, credentials read from disk)
// ---------------------------------------------------------------------------

/// Invoke `plugin get` with `{path, key, config}` on stdin; expect
/// `{"value": …}` on stdout and exit code 0.
pub async fn provider_get(
    plugin: &str,
    path: &str,
    key: &str,
    config: &serde_json::Value,
) -> Result<String> {
    let input = serde_json::json!({ "path": path, "key": key, "config": config });
    let mut child = tokio::process::Command::new(plugin)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Secret(format!("provider plugin {plugin:?}: spawn: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let body = serde_json::to_vec(&input)
            .map_err(|e| CoreError::Secret(format!("provider input encode: {e}")))?;
        let _ = stdin.write_all(&body).await;
        // Drop closes the pipe so plugins that read-to-EOF proceed.
    }
    let output = tokio::time::timeout(PROVIDER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| CoreError::Secret(format!("provider plugin {plugin:?} timed out")))?
        .map_err(|e| CoreError::Secret(format!("provider plugin {plugin:?}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        return Err(CoreError::Secret(format!(
            "provider plugin {plugin:?} failed (exit {:?}): {}",
            output.status.code(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| CoreError::Secret(format!("provider plugin {plugin:?}: bad JSON: {e}")))?;
    match parsed.get("value") {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
        None => Err(CoreError::Secret(format!(
            "provider plugin {plugin:?} returned no \"value\""
        ))),
    }
}

// ---------------------------------------------------------------------------
// Log redaction (MASTERPLAN §7.3: values registered with the log pipeline's
// redaction filter before spawn)
// ---------------------------------------------------------------------------

/// Streaming byte redactor: replaces every occurrence of registered secret
/// values with `[secret:label]` before bytes reach the log spool. Holds back
/// up to (longest value − 1) bytes between feeds so values that straddle
/// chunk boundaries are still caught.
#[derive(Debug)]
pub struct Redactor {
    /// (needle, replacement), longest needles first.
    patterns: Vec<(Vec<u8>, Vec<u8>)>,
    carry: Vec<u8>,
    hold: usize,
}

impl Redactor {
    /// Returns None when nothing needs redacting.
    pub fn new(redactions: &[(String, Vec<u8>)]) -> Option<Redactor> {
        let mut patterns: Vec<(Vec<u8>, Vec<u8>)> = redactions
            .iter()
            .filter(|(_, v)| v.len() >= REDACT_MIN_LEN)
            .map(|(label, v)| (v.clone(), format!("[secret:{label}]").into_bytes()))
            .collect();
        if patterns.is_empty() {
            return None;
        }
        patterns.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        let hold = patterns.first().map(|(n, _)| n.len() - 1).unwrap_or(0);
        Some(Redactor {
            patterns,
            carry: Vec::new(),
            hold,
        })
    }

    /// Feed a chunk; returns the redacted bytes that are safe to emit now.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let buf = self.replace_all(buf);
        let split = buf.len().saturating_sub(self.hold);
        self.carry = buf[split..].to_vec();
        buf[..split].to_vec()
    }

    /// Redact a whole string in one pass — the *document* form, next to
    /// the streaming one above.
    ///
    /// M18 lints every fact document through this before it can become
    /// replicated state (§9): a collector that dumps `/proc/*/environ` is
    /// the most obvious accident in that milestone, and an op-log entry is
    /// the worst possible place for a credential to land, because it
    /// replicates to every node and is never compacted.
    pub fn redact_text(&self, text: &str) -> String {
        let out = self.replace_all(text.as_bytes().to_vec());
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Flush held-back bytes at end of stream.
    pub fn finish(&mut self) -> Vec<u8> {
        let buf = std::mem::take(&mut self.carry);
        self.replace_all(buf)
    }

    fn replace_all(&self, mut buf: Vec<u8>) -> Vec<u8> {
        for (needle, replacement) in &self.patterns {
            buf = replace_bytes(&buf, needle, replacement);
        }
        buf
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A known-answer test against an independent implementation.
    ///
    /// Sealed secrets are *replicated and stored*: a node that seals one
    /// today must be able to open it after an upgrade, and a peer running a
    /// different build must be able to open it at all. A round-trip test
    /// passes happily while both halves are wrong together, so the vector
    /// below was produced by libsodium, not by this code.
    ///
    /// If this fails after a dependency bump, the AEAD's output changed and
    /// every secret in every mesh just became unreadable. Do not "fix" it by
    /// updating the expected bytes.
    #[test]
    fn the_aead_matches_an_independent_implementation() {
        use chacha20poly1305::aead::Aead;

        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let nonce: [u8; 24] = std::array::from_fn(|i| i as u8);
        let plaintext = b"meshware sealed secret";

        let sealed = aead(&key)
            .encrypt(&XNonce::from(nonce), plaintext.as_slice())
            .expect("encrypt");

        // libsodium: crypto_aead_xchacha20poly1305_ietf_encrypt, empty AAD.
        let expected = data_encoding::HEXLOWER
            .decode(b"f3a77c17e7b3ffcb133743afa737ccc838224bd798a23d5c0ba81db0e910d65472b7f902c22a")
            .expect("vector");
        assert_eq!(sealed, expected, "the sealed-secret format changed");

        let opened = aead(&key)
            .decrypt(&XNonce::from(nonce), expected.as_slice())
            .expect("decrypt the vector");
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_node_fails() {
        let node_a = Keypair::generate().unwrap();
        let node_b = Keypair::generate().unwrap();
        let stranger = Keypair::generate().unwrap();
        let user = UserId([9u8; 32]);
        let record = seal_secret(
            "db-pass",
            1,
            b"hunter2-hunter2",
            &[node_a.node_id(), node_b.node_id()],
            user,
        )
        .unwrap();

        assert_eq!(
            open_secret(&record, &node_a).unwrap().as_slice(),
            b"hunter2-hunter2"
        );
        assert_eq!(
            open_secret(&record, &node_b).unwrap().as_slice(),
            b"hunter2-hunter2"
        );
        assert!(open_secret(&record, &stranger).is_err());
    }

    #[test]
    fn rewrap_extends_recipients() {
        let node_a = Keypair::generate().unwrap();
        let late = Keypair::generate().unwrap();
        let user = UserId([9u8; 32]);
        let mut record =
            seal_secret("token", 1, b"secret-value", &[node_a.node_id()], user).unwrap();
        assert!(open_secret(&record, &late).is_err());
        let wraps = rewrap_for(&record, &node_a, &[late.node_id()]).unwrap();
        record.wraps.extend(wraps);
        assert_eq!(
            open_secret(&record, &late).unwrap().as_slice(),
            b"secret-value"
        );
    }

    #[test]
    fn a_planted_wrap_is_skipped_for_the_one_that_opens() {
        let node = Keypair::generate().unwrap();
        let user = UserId([9u8; 32]);
        let mut record = seal_secret("token", 1, b"secret-value", &[node.node_id()], user).unwrap();
        let junk = make_wraps(&[7u8; 32], &[node.node_id()]).unwrap();
        record.wraps.splice(0..0, junk);
        assert_eq!(
            open_secret(&record, &node).unwrap().as_slice(),
            b"secret-value"
        );
        record.wraps.remove(1);
        let err = open_secret(&record, &node).unwrap_err().to_string();
        assert!(err.contains("no wrap"), "{err}");
    }

    #[test]
    fn a_statement_marker_reads_back_and_is_not_a_plain_wrap() {
        let node = Keypair::generate().unwrap().node_id();
        let other = Keypair::generate().unwrap().node_id();
        let marker = statement_marker(node, 7);
        assert_eq!(
            read_statement(&node, std::slice::from_ref(&marker)),
            Some(7)
        );
        assert_eq!(read_statement(&other, &[marker]), None);
        let plain = make_wraps(&[1u8; 32], &[node]).unwrap();
        assert_eq!(read_statement(&node, &plain), None);
    }

    #[test]
    fn healers_are_few_and_widen_with_each_ask() {
        let user = UserId([9u8; 32]);
        let nodes: Vec<NodeId> = (0..40)
            .map(|_| Keypair::generate().unwrap().node_id())
            .collect();
        let record = seal_secret("token", 1, b"v", &nodes[..1], user).unwrap();
        let target = nodes[0];
        let count = |round| {
            nodes[1..]
                .iter()
                .filter(|me| is_healer(&record, &target, me, nodes[1..].iter().copied(), round))
                .count()
        };
        assert_eq!(count(0), SECRET_HEALERS);
        assert_eq!(count(1), SECRET_HEALERS * 2);
        assert_eq!(count(20), nodes.len() - 1);
    }

    #[test]
    fn ref_parsing() {
        // Scan order groups by marker: inline refs first, then file refs.
        let refs = find_refs(
            "x={{secret:db-pass}} y={{secret_file:tls.key}} z={{secret:vault:kv/data/app#token}}",
        );
        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0].source,
            SecretSource::Vault {
                name: "db-pass".into()
            }
        );
        assert!(!refs[0].file);
        assert_eq!(
            refs[1].source,
            SecretSource::Provider {
                provider: "vault".into(),
                path: "kv/data/app".into(),
                key: "token".into()
            }
        );
        assert!(!refs[1].file);
        assert_eq!(
            refs[2].source,
            SecretSource::Vault {
                name: "tls.key".into()
            }
        );
        assert!(refs[2].file);
        assert!(find_refs("no refs here {{secret:}} {{secret:unclosed").is_empty());
    }

    #[test]
    fn redactor_replaces_across_chunk_boundaries() {
        let mut r = Redactor::new(&[("tok".into(), b"SUPERSECRET".to_vec())]).unwrap();
        let mut out = Vec::new();
        out.extend(r.feed(b"prefix SUPERS"));
        out.extend(r.feed(b"ECRET suffix"));
        out.extend(r.finish());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "prefix [secret:tok] suffix"
        );
    }

    #[test]
    fn redactor_skips_short_values() {
        assert!(Redactor::new(&[("x".into(), b"ab".to_vec())]).is_none());
    }
}
