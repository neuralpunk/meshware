//! Finding and fetching ephemeral objects (MASTERPLAN M9 §5.5).
//!
//! Three steps, none of which involve the node that owns the job:
//!
//!   1. rank the directory nodes for the object id — every node computes
//!      the same ranking, so this is arithmetic, not a lookup;
//!   2. ask one of them who holds the bytes;
//!   3. fetch the manifest from a holder, then its chunks through the
//!      ordinary `GetChunk` path.
//!
//! That third step is why objects reuse the bundle store's transfer path
//! verbatim. A chunk hash is a global content address; the fetcher does
//! not need to know, and does not ask, which class of object it belongs to.
//!
//! There is no fallback to the origin, on purpose. If the directory has no
//! entry and no holder answers, the object is gone, and the answer to a
//! gone object is to re-run the task that produced it — inputs are
//! content-addressed and tasks are idempotent, which is exactly why the
//! bytes are k=1 in the first place (M9 §10, Q3).

use std::sync::Arc;

use mesh_proto::{JobId, NodeId, ObjectId, ObjectManifest, ObjectRef, Request, Response};
use tracing::debug;

use crate::error::{CoreError, Result};
use crate::objects::{ObjectStore, directory_for};
use crate::state::StateEngine;
use crate::transport::Transport;

/// Concurrent chunk fetches for one object. Matches the bundle path's
/// figure: the limiter is the connection pool, not this number (M9 §3.5).
const FETCH_CONCURRENCY: usize = 8;

/// Ask the object's directory nodes who holds it.
///
/// `hints` are holders the caller already believes in — the `ObjectRef` an
/// item outcome carried, typically. They are tried first and the directory
/// is only consulted when they do not answer, which makes the common case
/// (fetch what the report just told you about) cost zero directory traffic.
pub async fn locate(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    self_id: NodeId,
    object: &ObjectId,
    hints: &[NodeId],
) -> Vec<NodeId> {
    let members: Vec<NodeId> = engine.read(|s| {
        s.members
            .keys()
            .copied()
            .filter(|id| s.is_active_member(id))
            .collect()
    });
    let mut holders: Vec<NodeId> = hints.to_vec();
    for dir in directory_for(object, members) {
        if dir == self_id {
            // We are one of this object's directory nodes; the caller
            // consults the local directory itself before getting here.
            continue;
        }
        let addrs = engine.read(|s| s.members.get(&dir).map(|m| m.info.addrs.clone()));
        let Some(addrs) = addrs else { continue };
        let Ok(conn) = transport.member_conn(dir, &addrs).await else {
            continue;
        };
        match transport
            .request_one(&conn, &Request::ObjectLocate { object: *object })
            .await
        {
            Ok(Response::ObjectHolders { holders: found }) => {
                holders.extend(found.into_iter().map(|r: ObjectRef| r.holder));
            }
            Ok(_) => {}
            Err(e) => debug!(object = %object.short(), "directory lookup failed: {e}"),
        }
    }
    holders.retain(|h| *h != self_id);
    holders.dedup();
    holders
}

/// Fetch an object into the local store, from any node that has it.
///
/// Returns the manifest once every chunk has landed. Nothing is recorded
/// as held until then: a half-fetched object is not an object, and
/// announcing one would send the next fetcher somewhere useless.
pub async fn fetch(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    objects: &ObjectStore,
    self_id: NodeId,
    job: JobId,
    object: &ObjectId,
    holders: &[NodeId],
) -> Result<ObjectManifest> {
    if let Some(manifest) = objects.manifest(object)
        && objects.complete(&manifest)
    {
        return Ok(manifest);
    }
    let holders: Vec<NodeId> = holders.iter().copied().filter(|h| *h != self_id).collect();
    if holders.is_empty() {
        return Err(CoreError::Store(format!(
            "no holder for object {}",
            object.short()
        )));
    }

    let manifest = fetch_manifest(engine, transport, &holders, object).await?;
    if manifest.object_id() != *object {
        return Err(CoreError::Store(
            "holder returned a mismatched object manifest".into(),
        ));
    }

    let missing: Vec<mesh_proto::ChunkHash> = manifest
        .chunks
        .iter()
        .copied()
        .filter(|c| !objects.has_chunk(c))
        .collect();
    debug!(object = %object.short(), missing = missing.len(),
        holders = holders.len(), "fetching object");

    let mut pending = missing.into_iter();
    let mut inflight = futures::stream::FuturesUnordered::new();
    use futures::StreamExt;
    loop {
        while inflight.len() < FETCH_CONCURRENCY {
            let Some(chunk) = pending.next() else { break };
            let engine = engine.clone();
            let transport = transport.clone();
            let objects = objects.clone();
            let holders = holders.clone();
            inflight.push(async move {
                fetch_chunk(&engine, &transport, &objects, &holders, chunk).await
            });
        }
        match inflight.next().await {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }

    if !objects.complete(&manifest) {
        return Err(CoreError::Store(format!(
            "object {} still incomplete after fetch",
            object.short()
        )));
    }
    objects.adopt(job, manifest.clone());
    Ok(manifest)
}

async fn fetch_manifest(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    holders: &[NodeId],
    object: &ObjectId,
) -> Result<ObjectManifest> {
    let mut last = CoreError::Store(format!("no holder answered for object {}", object.short()));
    for holder in holders {
        let addrs = engine.read(|s| s.members.get(holder).map(|m| m.info.addrs.clone()));
        let Some(addrs) = addrs else { continue };
        let Ok(conn) = transport.member_conn(*holder, &addrs).await else {
            continue;
        };
        match transport
            .request_one(&conn, &Request::GetObject { object: *object })
            .await
        {
            Ok(Response::Object(Some(manifest))) => return Ok(*manifest),
            Ok(Response::Object(None)) => continue,
            Ok(Response::Error { message }) => last = CoreError::Store(message),
            Ok(other) => last = CoreError::Store(format!("unexpected: {other:?}")),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn fetch_chunk(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    objects: &ObjectStore,
    holders: &[NodeId],
    hash: mesh_proto::ChunkHash,
) -> Result<()> {
    if objects.has_chunk(&hash) {
        return Ok(());
    }
    for holder in holders {
        let addrs = engine.read(|s| s.members.get(holder).map(|m| m.info.addrs.clone()));
        let Some(addrs) = addrs else { continue };
        let Ok(conn) = transport.member_conn(*holder, &addrs).await else {
            continue;
        };
        match transport
            .request_one(&conn, &Request::GetChunk { hash })
            .await
        {
            // accept_chunk verifies the bytes against the hash.
            Ok(Response::Chunk { data }) if !data.is_empty() => {
                objects.accept_chunk(&hash, &data)?;
                return Ok(());
            }
            Ok(_) => continue,
            Err(e) => {
                debug!("object chunk fetch from {} failed: {e}", holder.short());
                continue;
            }
        }
    }
    Err(CoreError::Store(format!(
        "no holder returned object chunk {}",
        &hash.to_hex()[..12]
    )))
}
