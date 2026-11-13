//! M31 B4 — the bucket, and WORM for the audit log.
//!
//! Three claims, and the third is the one a compliance conversation turns
//! on:
//!
//! 1. **Re-running the replication uploads nothing.** Not "uploads less" —
//!    nothing. Part filenames are deterministic, so a part already in the
//!    bucket is the part we would write, and the idempotence that made
//!    compaction safe makes this free.
//! 2. **WORM means compliance mode, and only the audit table gets it.** A
//!    run record locked for a year is an operator who cannot fix a mistake;
//!    an audit row locked for a year is the point.
//! 3. **An exported audit row verifies with no meshware at all.** The
//!    `node` column *is* an Ed25519 public key, so the file carries its own
//!    verifier — which is what makes a WORM copy evidence rather than a
//!    copy nobody can check.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mesh_archive::Archive;
use mesh_archive::bucket::{BucketTarget, lock_days, push};
use mesh_proto::{AuditEntry, AuditId, Hlc, SignedAuditRow, UserId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MESH: &str = "mesh-b4b4b4b4";
const NOON: u64 = 1_772_452_800_000_000;

// ---------------------------------------------------------------------------
// A bucket, and a bucket with no Object Lock
// ---------------------------------------------------------------------------

/// Every PUT the fake store saw: key → (body length, object-lock headers).
type Puts = Arc<Mutex<HashMap<String, (usize, Vec<(String, String)>)>>>;

/// An S3-shaped store: HEAD answers from what it holds, PUT records the
/// object and its `x-amz-object-lock-*` headers, and `locks` decides
/// whether a locked PUT is accepted or refused the way a bucket without
/// Object Lock refuses one.
async fn fake_s3(locks: bool) -> (String, Puts) {
    let puts: Puts = Arc::new(Mutex::new(HashMap::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let Some((head, body_len)) = read_request(&mut sock).await else {
                    return;
                };
                let mut lines = head.lines();
                let start = lines.next().unwrap_or_default().to_string();
                let mut parts = start.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_string();
                let target = parts
                    .next()
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .filter(|(k, _)| k.starts_with("x-amz-object-lock"))
                    .collect();

                let response = match method.as_str() {
                    "PUT" => {
                        if !headers.is_empty() && !locks {
                            // What S3 actually says when a bucket has no
                            // Object Lock configuration.
                            let body = "<Error><Code>InvalidRequest</Code><Message>Bucket is \
                                        missing Object Lock Configuration</Message></Error>";
                            format!(
                                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\
                                 Connection: close\r\n\r\n{body}",
                                body.len()
                            )
                        } else {
                            seen.lock().unwrap().insert(target, (body_len, headers));
                            "HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                                .to_string()
                        }
                    }
                    "HEAD" => match seen.lock().unwrap().get(&target) {
                        Some((len, _)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\
                             Last-Modified: Wed, 04 Mar 2026 02:00:00 GMT\r\n\
                             ETag: \"x\"\r\nConnection: close\r\n\r\n"
                        ),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                                 Connection: close\r\n\r\n"
                            .to_string(),
                    },
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), puts)
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, usize)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let want: usize = head
        .to_ascii_lowercase()
        .split("content-length:")
        .nth(1)
        .and_then(|r| r.split("\r\n").next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut have = buf.len() - split;
    while have < want {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        have += n;
    }
    Some((head, want))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn node() -> mesh_core::identity::Keypair {
    mesh_core::identity::Keypair::generate().unwrap()
}

/// One genuinely signed audit row — signed the way the daemon signs it, so
/// the verification under test is the real one rather than a rehearsal.
fn signed_row(
    key: &mesh_core::identity::Keypair,
    seq: u64,
    at: u64,
    action: &str,
) -> SignedAuditRow {
    let entry = AuditEntry {
        audit_id: AuditId::generate(),
        at_micros: at,
        user: UserId([7; 32]),
        username: "ada".into(),
        action: action.into(),
        target: "job-1".into(),
        node: key.node_id(),
        outcome: "ok".into(),
        detail: String::new(),
        request_sig: vec![9; 64],
    };
    let body = postcard::to_stdvec(&mesh_proto::OpBody::AuditAppend {
        entry: entry.clone(),
    })
    .unwrap();
    let hlc = Hlc {
        wall_micros: at,
        logical: 0,
    };
    let signing_bytes = mesh_proto::op_signing_bytes_raw(&key.node_id(), seq, &hlc, &body);
    SignedAuditRow {
        entry,
        author: key.node_id(),
        seq,
        hlc,
        signature: key.sign(&signing_bytes).to_vec(),
        signing_bytes,
    }
}

async fn seeded_archive(dir: &std::path::Path) -> (Archive, mesh_core::identity::Keypair) {
    let key = node();
    let archive = Archive::for_mesh(dir.join("archive"), MESH).unwrap();
    let rows: Vec<SignedAuditRow> = (0..4)
        .map(|i| signed_row(&key, i + 1, NOON + i * 1_000_000, "job.run"))
        .collect();
    archive.ingest_audit(&rows).unwrap();
    (archive, key)
}

fn target_for(endpoint: &str) -> BucketTarget {
    BucketTarget::parse("s3://acme-archive/prod")
        .unwrap()
        .with_endpoint(Some(endpoint.to_string()))
        .with_region(Some("us-east-1".into()))
}

fn store_for(target: &BucketTarget) -> object_store_client::Client {
    target
        .client(Some("key"), Some("secret"), true, None)
        .unwrap()
}

fn locked_store_for(target: &BucketTarget, days: u32) -> object_store_client::Client {
    target
        .client(Some("key"), Some("secret"), true, Some(days))
        .unwrap()
}

/// `object_store`'s S3 type, named once so the tests do not repeat it.
mod object_store_client {
    pub type Client = object_store::aws::AmazonS3;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **Re-running uploads nothing.** Not less — nothing.
///
/// Part filenames come from `Manifest.next_part` and are never reused, so a
/// part in the bucket is byte-identical to the one on disk. That is why
/// there is no upload manifest, no etag bookkeeping and no `--resume`:
/// running this hourly and running it once a week differ only in freshness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replicating_twice_uploads_nothing_the_second_time() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, _key) = seeded_archive(dir.path()).await;
    let (endpoint, puts) = fake_s3(true).await;
    let target = target_for(&endpoint);
    let store = store_for(&target);

    let first = push(archive.root(), &target, &store, None).await.unwrap();
    assert!(first.uploaded > 0, "nothing was uploaded at all: {first:?}");
    assert_eq!(first.already_there, 0);
    let after_first = puts.lock().unwrap().len();

    let second = push(archive.root(), &target, &store, None).await.unwrap();
    assert_eq!(second.uploaded, 0, "a second pass re-uploaded {second:?}");
    assert_eq!(second.already_there, first.uploaded);
    assert_eq!(
        puts.lock().unwrap().len(),
        after_first,
        "the bucket grew on a pass that should have written nothing"
    );

    // And the manifest went up too: a bucket holding parts and no manifest
    // is a bucket a reader cannot tell is complete.
    assert!(
        // Path-style addressing puts the bucket in the path, which is what
        // a custom endpoint (MinIO, Ceph) always uses.
        puts.lock()
            .unwrap()
            .keys()
            .any(|k| k == "acme-archive/prod/manifest.json"),
        "{:?}",
        puts.lock().unwrap().keys().collect::<Vec<_>>()
    );
}

/// **Compliance mode, on the audit table, and nowhere else.**
///
/// Governance mode is bypassable by anyone holding
/// `s3:BypassGovernanceRetention` — usually the account that made the
/// bucket. Writing it and calling the feature WORM would be a compliance
/// claim the storage does not support.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worm_locks_the_audit_table_in_compliance_mode_and_leaves_the_rest_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, _key) = seeded_archive(dir.path()).await;
    // An empty ingest, so the pass runs over a directory that also holds
    // tables the lock must leave alone.
    archive.ingest(&[], NOON + 30 * 86_400 * 1_000_000).unwrap();

    let (endpoint, puts) = fake_s3(true).await;
    let target = target_for(&endpoint);
    let store = store_for(&target);

    let locked = locked_store_for(&target, lock_days(0));
    let report = push(
        archive.root(),
        &target,
        &store,
        Some((&locked, lock_days(0))),
    )
    .await
    .unwrap();
    assert!(report.locked > 0, "nothing was locked: {report:?}");

    let seen = puts.lock().unwrap().clone();
    let mut checked_audit = false;
    for (key, (_, headers)) in &seen {
        let locked: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        if key.contains("/audit/") {
            checked_audit = true;
            let mode = headers
                .iter()
                .find(|(k, _)| k == "x-amz-object-lock-mode")
                .map(|(_, v)| v.as_str());
            assert_eq!(
                mode,
                Some("COMPLIANCE"),
                "an audit object must be locked in compliance mode, not {mode:?}"
            );
            assert!(
                headers
                    .iter()
                    .any(|(k, _)| k == "x-amz-object-lock-retain-until-date"),
                "a locked object needs a retain-until date"
            );
        } else {
            assert!(
                locked.is_empty(),
                "{key} is not an audit object and must not be locked: {locked:?}"
            );
        }
    }
    assert!(
        checked_audit,
        "no audit object reached the bucket: {seen:?}"
    );

    // A year at least, whatever the operator asked for — a WORM copy that
    // expired before the mesh's own audit window would be worse than none.
    assert_eq!(lock_days(0), mesh_proto::AUDIT_FLOOR_DAYS);
}

/// A bucket without Object Lock **refuses the write**, and says what S3
/// said. A silent downgrade to an ordinary object would turn "this cannot
/// be altered" into a sentence an auditor will find is false.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bucket_without_object_lock_refuses_rather_than_writing_an_ordinary_object() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, _key) = seeded_archive(dir.path()).await;
    let (endpoint, puts) = fake_s3(false).await;
    let target = target_for(&endpoint);
    let store = store_for(&target);

    let locked = locked_store_for(&target, 365);
    let err = push(archive.root(), &target, &store, Some((&locked, 365)))
        .await
        .expect_err("a WORM push to a bucket with no Object Lock must fail")
        .to_string();
    // **The store's own words, not only our wrapper's.** The wrapper adds
    // "Object Lock" to every failure of a locked PUT, so asserting on that
    // alone would let a client-side error — a dropped `allow_http`, a
    // malformed header — pass as a successful policy refusal. That happened
    // once while this was being written, which is why the assertion is
    // specific.
    assert!(
        err.contains("Object Lock Configuration"),
        "the refusal must carry what S3 said: {err}"
    );
    assert!(
        err.contains("never afterwards"),
        "and must say a bucket cannot gain Object Lock later: {err}"
    );

    // And no audit object was written unlocked as a consolation.
    let seen = puts.lock().unwrap();
    assert!(
        !seen.keys().any(|k| k.contains("/audit/")),
        "an audit object was written despite the refusal: {:?}",
        seen.keys().collect::<Vec<_>>()
    );
}

/// **The row carries its own verifier.**
///
/// `node` is a `NodeId`, a `NodeId` *is* an Ed25519 public key, and
/// `signing_bytes` is what was signed. So an auditor holding nothing but
/// this file — pulled out of a bucket, onto a machine that has never been
/// in the mesh — can check every row. That is the difference between an
/// audit trail and a table of claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exported_audit_row_verifies_with_nothing_but_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, _key) = seeded_archive(dir.path()).await;

    let proofs = archive.audit_proofs().await.unwrap();
    assert_eq!(proofs.len(), 4, "{proofs:?}");
    for proof in &proofs {
        let node = mesh_proto::NodeId::parse(&proof.node).expect("the node column is a key");
        let sig: [u8; 64] = proof.signature.as_slice().try_into().expect("64 bytes");
        assert!(
            mesh_core::identity::verify_signature(&node.0, &proof.signing_bytes, &sig),
            "row {} does not verify against the key in its own node column",
            proof.audit_id
        );
    }

    // And a tampered row fails — which is the assertion that makes the one
    // above worth making.
    let mut tampered = proofs[0].clone();
    tampered.signing_bytes[8] ^= 0xff;
    let node = mesh_proto::NodeId::parse(&tampered.node).unwrap();
    let sig: [u8; 64] = tampered.signature.as_slice().try_into().unwrap();
    assert!(
        !mesh_core::identity::verify_signature(&node.0, &tampered.signing_bytes, &sig),
        "an edited row must not verify"
    );
}

/// An audit row whose signed bytes do not encode its own entry is refused
/// at ingest, not written and shrugged at. Writing it into an immutable
/// bucket would put a lie somewhere nobody can delete it from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_that_does_not_bind_its_own_entry_never_reaches_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let key = node();
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).unwrap();

    let good = signed_row(&key, 1, NOON, "job.run");
    let mut bad = signed_row(&key, 2, NOON + 1_000_000, "job.run");
    // The signature still checks out — it is over these bytes — but the
    // bytes are somebody else's row. Exactly the swap a naive verifier
    // that only checked the signature would wave through.
    bad.signing_bytes = good.signing_bytes.clone();
    bad.signature = key.sign(&bad.signing_bytes).to_vec();

    let report = archive.ingest_audit(&[good, bad]).unwrap();
    assert_eq!(report.audit_written, 1);
    assert_eq!(report.audit_refused, 1);

    let proofs = archive.audit_proofs().await.unwrap();
    assert_eq!(proofs.len(), 1, "the unbound row reached the archive");
}

/// Re-ingesting the same audit history writes nothing: the watermark is
/// over the entry instant, exactly as every other table's is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_ingesting_the_audit_trail_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let key = node();
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).unwrap();
    let rows: Vec<SignedAuditRow> = (0..3)
        .map(|i| signed_row(&key, i + 1, NOON + i * 1_000_000, "job.run"))
        .collect();

    assert_eq!(archive.ingest_audit(&rows).unwrap().audit_written, 3);
    let again = archive.ingest_audit(&rows).unwrap();
    assert_eq!(again.audit_written, 0);
    assert_eq!(again.audit_skipped, 3);
}
