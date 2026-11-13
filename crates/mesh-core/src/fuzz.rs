//! Entry points for fuzzing the code that reads bytes somebody else chose
//! (SEC-22).
//!
//! Each function takes arbitrary bytes and must return — an error is fine,
//! a panic is the finding. They live in the library, beside the code they
//! drive, so a target can reach a parser that is not public and cannot
//! drift from the code it tests. Two things run them:
//!
//! * `fuzz/` at the repository root, a `cargo fuzz` project with one
//!   target per function, for coverage-guided runs on a nightly toolchain;
//! * `tests/fuzz_smoke.rs`, on stable, which runs every target over seed
//!   inputs and a few thousand random and mutated ones on every test run.
//!
//! The one that matters most is [`op_apply`]. Merging runs under the state
//! write lock on every node an op reaches, so a panic there used to poison
//! the lock and take each of those nodes down; `StateEngine::try_apply`
//! now contains one, and this target is how the next one is found.

use parking_lot::Mutex;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};

use mesh_proto::{
    Hlc, IpcRequest, IpcResponse, MeshId, NodeId, OpBody, OpEnvelope, Request, Response, SignedIpc,
};

use crate::genesis::Genesis;
use crate::identity::Keypair;
use crate::state::StateEngine;

/// Largest input worth decoding. Every reader here caps what it accepts
/// well below this; the cap keeps a fuzzer's corpus from wandering off.
const MAX_INPUT: usize = 1 << 20;

/// Poll a future that never waits — a reader over a byte slice — once.
fn ready<F: Future>(f: F) -> Option<F::Output> {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(out) => Some(out),
        Poll::Pending => None,
    }
}

/// A wire frame, as a peer's stream delivers it: length prefix, then a
/// postcard body, read as each of the shapes a node reads.
pub fn wire_frame(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let mut r = data;
    let _ = ready(mesh_proto::read_frame::<_, Request>(&mut r));
    let mut r = data;
    let _ = ready(mesh_proto::read_frame_limited::<_, Request>(
        &mut r,
        256 * 1024,
    ));
    let mut r = data;
    let _ = ready(mesh_proto::read_frame::<_, Response>(&mut r));
    let mut r = data;
    let _ = ready(mesh_proto::read_frame::<_, Vec<OpEnvelope>>(&mut r));
    if let Ok(req) = postcard::from_bytes::<Request>(data) {
        let _ = (req.name(), req.min_minor(), req.requires_membership());
    }
}

/// A signed IPC envelope, as the local socket and the HTTP API receive it.
pub fn ipc_envelope(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(env) = postcard::from_bytes::<SignedIpc>(data) else {
        let _ = postcard::from_bytes::<IpcRequest>(data);
        let _ = postcard::from_bytes::<IpcResponse>(data);
        return;
    };
    let node = NodeId([3u8; 32]);
    let _ = crate::identity::verify_ipc(&env, MeshId([9u8; 16]), &[node]);
    if let Ok(req) = postcard::from_bytes::<IpcRequest>(&env.payload) {
        let _ = serde_json::to_string(&req);
    }
}

/// An op as gossip delivers it, before anything is trusted.
pub fn op_envelope(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    for op in postcard::from_bytes::<Vec<OpEnvelope>>(data)
        .unwrap_or_default()
        .into_iter()
        .chain(postcard::from_bytes::<OpEnvelope>(data).ok())
    {
        let _ = op.body_index();
        let _ = op.signing_bytes();
        if let Some(body) = op.decode_body() {
            let _ = (body.kind(), body.topic());
            let _ = crate::compact::compaction(&body, &Default::default());
        }
    }
    if let Ok(body) = postcard::from_bytes::<OpBody>(data) {
        let _ = (body.kind(), body.topic());
    }
}

/// A throwaway engine whose founder node signs whatever the fuzzer hands
/// it, so the bytes reach the merge rules rather than stopping at the
/// signature check. Rebuilt every so often, so state does not grow for
/// the length of a fuzzing run.
struct Scratch {
    dir: PathBuf,
    engine: Arc<StateEngine>,
    key: Keypair,
    seq: u64,
    tick: u64,
}

const SCRATCH_OPS: u64 = 5_000;

fn scratch() -> &'static Mutex<Option<Scratch>> {
    static SCRATCH: OnceLock<Mutex<Option<Scratch>>> = OnceLock::new();
    SCRATCH.get_or_init(|| Mutex::new(None))
}

fn new_scratch() -> Option<Scratch> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("mw-fuzz-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let user = Keypair::generate().ok()?;
    let key = Keypair::generate().ok()?;
    let genesis = Genesis::create("fuzz", &user, key.node_id()).ok()?;
    let engine = StateEngine::open(&dir, key.clone(), genesis).ok()?;
    Some(Scratch {
        dir,
        engine,
        key,
        seq: 0,
        tick: 0,
    })
}

/// An op body, signed by a node the engine accepts, merged.
pub fn op_apply(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(body) = postcard::from_bytes::<OpBody>(data) else {
        return;
    };
    with_scratch(|s| apply_one(s, &body));
}

fn with_scratch(f: impl FnOnce(&mut Scratch)) {
    let mut guard = scratch().lock();
    if guard.as_ref().is_none_or(|s| s.tick >= SCRATCH_OPS) {
        if let Some(old) = guard.take() {
            let _ = std::fs::remove_dir_all(&old.dir);
        }
        *guard = new_scratch();
    }
    if let Some(s) = guard.as_mut() {
        f(s);
    }
}

fn apply_one(s: &mut Scratch, body: &OpBody) {
    // The next sequence number as the engine counts it: an op the merge
    // ignored or parked does not move the watermark, and numbering past
    // it would park every op after it on the gap.
    let author = s.key.node_id();
    s.seq = s
        .engine
        .read(|st| st.applied.get(&author).copied().unwrap_or(0))
        + 1;
    s.tick += 1;
    let hlc = Hlc {
        wall_micros: mesh_proto::now_micros(),
        logical: u32::try_from(s.tick % u64::from(u32::MAX)).unwrap_or(0),
    };
    let Ok(bytes) = postcard::to_stdvec(body) else {
        return;
    };
    let signature = s
        .key
        .sign(&mesh_proto::op_signing_bytes_raw(
            &author, s.seq, &hlc, &bytes,
        ))
        .to_vec();
    let op = OpEnvelope {
        author,
        seq: s.seq,
        hlc,
        body: bytes,
        signature,
    };
    let contained = s.engine.contained_panics();
    let _ = s.engine.apply_unpersisted(vec![op]);
    // The engine contains a panicking merge so a node survives one; a
    // fuzzer has to be told, or it would never find one.
    assert_eq!(
        s.engine.contained_panics(),
        contained,
        "merging {} panicked",
        body.kind()
    );
    // Reading it back is part of it: a merge that left state a reader
    // trips over is the same outage by another road.
    s.engine.read(|st| {
        let _ = st.members.len();
        let _ = st.invite_uses_left(&mesh_proto::InviteId([0; 8]));
    });
}

/// A bundle manifest, as a peer serves it.
pub fn manifest(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    if let Ok(m) = postcard::from_bytes::<mesh_proto::content::Manifest>(data) {
        let _ = m.validate();
    }
    if let Ok(m) = postcard::from_bytes::<mesh_proto::ObjectManifest>(data) {
        let _ = (m.object_id(), m.is_empty());
    }
}

/// What an HTTP server sends this node's client: a head, a chunked body, a
/// redirect.
pub fn http_response(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = crate::httpc::parse_head(data);
    let _ = crate::httpc::dechunk(data);
    let text = String::from_utf8_lossy(data);
    let _ = crate::httpc::resolve_location("https://idp.example/a/b", &text);
    let _ = crate::httpc::host_of(&text);
    let _ = crate::httpc::is_private_host(&text);
}

/// An ID token, as an identity provider hands it back.
pub fn jwt(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    static KEYS: OnceLock<Vec<crate::oidc::Jwk>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| {
        [
            r#"{"kid":"ec","kty":"EC","crv":"P-256","x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU","y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"}"#,
            r#"{"kid":"rsa","kty":"RSA","n":"sXchDaQebHnPiGvyDOAT4saGEUetSyo9MKLOoWFsueri23bOdgWp4Dy1WlUzewbgBHod5pcM9H95GQRV3JDXboIRROSBigeC5yjU1hGzHHyXss8UDprecbAYxknTcQkhslANGRUZmdTOQ5qTRsLAt6BTYuyvVRdhS8exSZEy_c4gs_7svlJJQ4H9_NxsiIoLwAEk7-Q3UXERGYw_75IDrGA84-lA_-Ct4eTlXHBIY2EaV7t7LjJaynVJCpkv4LKjTTAumiGUIuQhrNhZLuF_RJLqHpM2kgWFLU7-VTdL1VbC2tejvcI2BlMkEpk1BzBZI0KQB0GaDWFLN-aEAw3vRw","e":"AQAB"}"#,
        ]
        .iter()
        .filter_map(|j| serde_json::from_str(j).ok())
        .collect()
    });
    let provider = mesh_proto::OidcProvider {
        name: "fuzz".into(),
        issuer: "https://idp.example".into(),
        client_id: "meshware".into(),
        jwks_uri: "https://idp.example/keys".into(),
        device_authorization_endpoint: "https://idp.example/device".into(),
        token_endpoint: "https://idp.example/token".into(),
        scopes: vec!["openid".into()],
        groups_claim: "groups".into(),
        group_roles: vec![],
        configured_by: mesh_proto::UserId([1; 32]),
        configured_at_micros: 0,
        signature: vec![],
    };
    let token = String::from_utf8_lossy(data);
    for key in keys {
        let _ = crate::oidc::verify_with_key(
            &provider,
            &token,
            &mesh_proto::UserId([2; 32]),
            NodeId([3; 32]),
            1_700_000_000,
            key,
        );
    }
}

/// A hardware attestation chain, as a person submits it.
pub fn attestation(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(att) = postcard::from_bytes::<mesh_proto::HardwareAttestation>(data) else {
        return;
    };
    // Its own last certificate as the root, so a chain gets as far as the
    // link checks, and a junk root beside it.
    let mut roots = vec![("junk".to_string(), "junk".to_string(), data.to_vec())];
    if let Some(last) = att.chain.last() {
        roots.push(("self".into(), "self".into(), last.clone()));
    }
    let _ = crate::attest::verify(&att, &roots);
}

/// What an SSH agent (the hardware-key path) and an SSHSIG signature say.
pub fn agent_reply(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let _ = crate::hardware::parse_identities(data);
    let _ = crate::hardware::parse_signature(data);
    let text = String::from_utf8_lossy(data);
    if let Ok(sig) = crate::sshsig::parse(&text) {
        let _ = crate::sshsig::verify(&sig, b"message", "meshware");
        let _ = crate::sshsig::key_line_matches(&sig, "ssh-ed25519 AAAA");
    }
}

/// A DNS query, as the registry's resolver receives it — the parse, and
/// the answer built from it.
pub fn dns_query(data: &[u8]) {
    if data.len() > 64 * 1024 {
        return;
    }
    if let Some(q) = crate::dns::parse_query(data) {
        let _ = crate::dns::build_reply(&q, &[], false, true);
        let _ = crate::dns::build_reply(&q, &[], true, false);
    }
    with_scratch(|s| {
        let _ = crate::dns::answer(&s.engine, data, true, &|_| false);
        let _ = crate::dns::answer(&s.engine, data, false, &|_| true);
    });
}

/// One target: bytes in, a panic out if something is wrong.
pub type Target = fn(&[u8]);

/// Every target, by name, for a harness that runs them all.
pub const TARGETS: &[(&str, Target)] = &[
    ("wire_frame", wire_frame),
    ("ipc_envelope", ipc_envelope),
    ("op_envelope", op_envelope),
    ("op_apply", op_apply),
    ("manifest", manifest),
    ("http_response", http_response),
    ("jwt", jwt),
    ("attestation", attestation),
    ("agent_reply", agent_reply),
    ("dns_query", dns_query),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The harness is only worth anything if its ops reach the merge.
    #[test]
    fn an_op_reaches_the_merge() {
        let body = OpBody::MemberUpdate {
            node_id: NodeId([8; 32]),
            name: "x".into(),
            addrs: vec![],
            tags: vec![],
            slots: 1,
        };
        // Applied and read back under one hold of the scratch lock: the
        // other test here shares the engine, and a refused op of its
        // landing in between would move `seq` without moving the mark.
        let mut checked = false;
        with_scratch(|s| {
            apply_one(s, &body);
            let applied = s
                .engine
                .read(|st| st.applied.get(&s.key.node_id()).copied().unwrap_or(0));
            assert_eq!(applied, s.seq, "the op was not applied");
            checked = true;
        });
        assert!(checked, "the scratch engine did not open");
    }

    /// An op the merge refuses must not stop the ones after it.
    #[test]
    fn a_refused_op_does_not_block_the_next() {
        let refused = OpBody::JobUpsert {
            summary: mesh_proto::JobSummary {
                job_id: mesh_proto::JobId([1; 16]),
                cmd_preview: String::new(),
                runtime: String::new(),
                submitter: NodeId([9; 32]),
                submitter_user: mesh_proto::UserId([9; 32]),
                // Not the author: ignored.
                executor: NodeId([9; 32]),
                state: mesh_proto::JobState::Running,
                exit_code: None,
                created_at_micros: 0,
                started_at_micros: None,
                finished_at_micros: None,
                fanout: None,
                updated_hlc: Hlc::default(),
            },
        };
        op_apply(&postcard::to_stdvec(&refused).expect("encode"));
        an_op_reaches_the_merge();
    }
}
