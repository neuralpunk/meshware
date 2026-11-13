//! Bundle fetching over the mesh (MASTERPLAN §8.1, §11).
//!
//! To obtain a bundle a node lacks: pull the manifest from any holder, then
//! fetch missing chunks — in parallel, from whichever holders answer. Every
//! chunk is verified against its hash on write, so a bad peer can't corrupt
//! the store. This is the lazy-replication path that makes "Git outage =
//! execution outage" impossible: content lives in the mesh, not one server.

use std::sync::Arc;

use mesh_proto::{BundleId, ChunkHash, Manifest, NodeId, Request, Response};
use tracing::{debug, warn};

use crate::content::ContentStore;
use crate::error::{CoreError, Result};
use crate::state::StateEngine;
use crate::transport::Transport;

/// Max concurrent chunk fetches per bundle.
const FETCH_CONCURRENCY: usize = 8;

/// How many times a fetch re-reads the holder set and tries again for what
/// is still missing.
///
/// Chunks are content-addressed and written as they arrive, so a retry
/// costs only what did not land — and the thing being retried is usually
/// not a broken bundle but a *moving fleet*: a wave of nodes upgrading
/// together means holders that drain, exit and come back inside the window
/// of a single fetch. One pass through a holder list captured before that
/// started is not a fair test of whether the content is reachable.
const FETCH_ROUNDS: usize = 4;

/// Ensure a bundle is fully present locally, fetching from holders if needed.
/// Returns immediately if already complete.
///
/// `extra_holders` are candidate holders to try even if replicated state
/// doesn't yet list them — e.g. a job's submitter, which resolved the service
/// and reliably holds the content. This makes remote execution robust against
/// gossip lag (the holder op may not have reached this node yet).
///
/// `alive` says which holders are currently up. It is advisory and never
/// exclusive: a node believed dead is tried *last* rather than not at all,
/// because liveness is a gossip reading a few seconds old and the holder
/// that has the bytes is worth a connection attempt regardless.
pub async fn ensure_bundle(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    content: &ContentStore,
    self_id: NodeId,
    bundle_id: &BundleId,
    extra_holders: &[NodeId],
    alive: &(dyn Fn(NodeId) -> bool + Send + Sync),
) -> Result<()> {
    if content.has_bundle(bundle_id) {
        return Ok(());
    }

    let mut last_error: Option<CoreError> = None;
    for round in 0..FETCH_ROUNDS {
        // Re-read holders every round rather than once. During a wave the
        // set genuinely changes: each node that finishes announces itself
        // as a holder, so the second round of a fetch usually has more
        // places to pull from than the first — and a peer that was
        // mid-restart is alive again.
        let holders = candidate_holders(engine, self_id, bundle_id, extra_holders, alive);
        if holders.is_empty() {
            return Err(CoreError::Store(format!(
                "no reachable holder for bundle {}",
                bundle_id.short()
            )));
        }

        let manifest = match content.load_manifest(bundle_id)? {
            Some(m) => m,
            None => {
                let manifest = fetch_manifest(engine, transport, &holders, bundle_id).await?;
                if manifest.bundle_id() != *bundle_id {
                    return Err(CoreError::Store(
                        "holder returned a mismatched manifest".into(),
                    ));
                }
                content.save_manifest(&manifest)?;
                manifest
            }
        };

        let missing = content.missing_chunks(&manifest);
        if missing.is_empty() {
            break;
        }
        debug!(
            bundle = %bundle_id.short(),
            missing = missing.len(),
            holders = holders.len(),
            round,
            "fetching bundle"
        );

        let mut pending = missing.into_iter().enumerate();
        let mut inflight = futures::stream::FuturesUnordered::new();
        let mut failures = 0usize;
        use futures::StreamExt;

        loop {
            while inflight.len() < FETCH_CONCURRENCY {
                let Some((i, chunk)) = pending.next() else {
                    break;
                };
                let engine = engine.clone();
                let transport = transport.clone();
                let content = content.clone();
                let holders = holders.clone();
                inflight.push(async move {
                    // Each chunk starts at a different holder. Starting
                    // every chunk at the same one turns a single slow or
                    // dead peer into a timeout paid once per chunk, and
                    // leaves the rest of the fleet idle while one node
                    // serves everything.
                    fetch_chunk(&engine, &transport, &content, &holders, i, chunk).await
                });
            }
            match inflight.next().await {
                // A chunk nobody could serve does not discard the bundle.
                // It used to: one failure returned, throwing away every
                // chunk still in flight and every one already queued, so a
                // fetch racing a restarting peer lost a whole pass over a
                // single miss.
                Some(Err(e)) => {
                    failures += 1;
                    last_error = Some(e);
                }
                Some(Ok(())) => {}
                None => break,
            }
        }

        if failures == 0 {
            break;
        }
        warn!(
            bundle = %bundle_id.short(),
            failures,
            round,
            "some chunks could not be fetched this round — retrying what is left"
        );
        // Short, bounded backoff. The common cause is a holder that is
        // restarting, and that resolves in seconds.
        tokio::time::sleep(std::time::Duration::from_millis(500 * (round as u64 + 1))).await;
    }

    if !content.has_bundle(bundle_id) {
        return Err(match last_error {
            Some(e) => CoreError::Store(format!(
                "bundle {} still incomplete after {FETCH_ROUNDS} rounds: {e}",
                bundle_id.short()
            )),
            None => CoreError::Store("bundle still incomplete after fetch".into()),
        });
    }
    Ok(())
}

/// Holders worth asking, best first: alive before believed-dead, and never
/// this node.
fn candidate_holders(
    engine: &Arc<StateEngine>,
    self_id: NodeId,
    bundle_id: &BundleId,
    extra_holders: &[NodeId],
    alive: &(dyn Fn(NodeId) -> bool + Send + Sync),
) -> Vec<NodeId> {
    let mut ranked: Vec<(u8, NodeId)> = engine.read(|s| {
        s.bundle_holders(bundle_id)
            .into_iter()
            .chain(extra_holders.iter().copied())
            .filter(|n| *n != self_id && s.is_active_member(n))
            .map(|n| {
                // Three tiers, best first:
                //
                // 0. An explicit hint — the job's submitter, the node that
                //    resolved the service. It is *known* to hold the bytes,
                //    which replicated state may not say yet, and demoting
                //    it to a gossip reading would trade a certainty for an
                //    inference.
                // 1. A holder gossip currently calls alive.
                // 2. Everyone else. Still tried: liveness is a few seconds
                //    old, and a node with the bytes is worth one attempt
                //    however it is currently believed to be doing.
                let rank = if extra_holders.contains(&n) {
                    0
                } else if alive(n) {
                    1
                } else {
                    2
                };
                (rank, n)
            })
            .collect()
    });
    // By rank, then by id so the order is deterministic within a tier.
    ranked.sort();
    ranked.dedup_by_key(|(_, n)| *n);
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .map(|(_, n)| n)
        .filter(|n| seen.insert(*n))
        .collect()
}

/// Ensure a bundle's *manifest* is present locally — fetched from a holder
/// if needed, no chunks — and return it. This is what browsing a bundle's
/// files costs (`meshnode bundles show`, Director's file picker): a few
/// kilobytes, not the payload.
pub async fn ensure_manifest(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    content: &ContentStore,
    self_id: NodeId,
    bundle_id: &BundleId,
) -> Result<Manifest> {
    if let Some(m) = content.load_manifest(bundle_id)? {
        return Ok(m);
    }
    let mut holders: Vec<NodeId> = engine.read(|s| {
        s.bundle_holders(bundle_id)
            .into_iter()
            .filter(|n| *n != self_id && s.is_active_member(n))
            .collect()
    });
    holders.sort();
    holders.dedup();
    if holders.is_empty() {
        return Err(CoreError::Store(format!(
            "no reachable holder for bundle {}",
            bundle_id.short()
        )));
    }
    let manifest = fetch_manifest(engine, transport, &holders, bundle_id).await?;
    if manifest.bundle_id() != *bundle_id {
        return Err(CoreError::Store(
            "holder returned a mismatched manifest".into(),
        ));
    }
    content.save_manifest(&manifest)?;
    Ok(manifest)
}

async fn fetch_manifest(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    holders: &[NodeId],
    bundle_id: &BundleId,
) -> Result<Manifest> {
    let mut last = CoreError::Store("no holder answered for manifest".into());
    for holder in holders {
        let addrs = engine.read(|s| s.members.get(holder).map(|m| m.info.addrs.clone()));
        let Some(addrs) = addrs else { continue };
        match transport.member_conn(*holder, &addrs).await {
            Ok(conn) => {
                match transport
                    .request_one(
                        &conn,
                        &Request::GetManifest {
                            bundle_id: *bundle_id,
                        },
                    )
                    .await
                {
                    Ok(Response::Manifest(m)) => return Ok(*m),
                    Ok(Response::Error { message }) => last = CoreError::Store(message),
                    Ok(other) => last = CoreError::Store(format!("unexpected: {other:?}")),
                    Err(e) => last = e,
                }
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn fetch_chunk(
    engine: &Arc<StateEngine>,
    transport: &Arc<Transport>,
    content: &ContentStore,
    holders: &[NodeId],
    start_at: usize,
    hash: ChunkHash,
) -> Result<()> {
    if content.has_chunk(&hash) {
        return Ok(());
    }
    // Every holder is still tried; only the order rotates. A miss costs a
    // connection attempt, and paying it on the same peer for every chunk is
    // what made one restarting holder look like a stalled fetch.
    let offset = if holders.is_empty() {
        0
    } else {
        start_at % holders.len()
    };
    for holder in holders.iter().cycle().skip(offset).take(holders.len()) {
        let addrs = engine.read(|s| s.members.get(holder).map(|m| m.info.addrs.clone()));
        let Some(addrs) = addrs else { continue };
        let conn = match transport.member_conn(*holder, &addrs).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        match transport
            .request_one(&conn, &Request::GetChunk { hash })
            .await
        {
            Ok(Response::Chunk { data }) if !data.is_empty() => {
                // write_chunk verifies the hash.
                content.write_chunk(&hash, &data)?;
                return Ok(());
            }
            Ok(_) => continue, // holder didn't have it; try next
            Err(e) => {
                warn!("chunk fetch from {} failed: {e}", holder.short());
                continue;
            }
        }
    }
    Err(CoreError::Store(format!(
        "no holder returned chunk {}",
        &hash.to_hex()[..12]
    )))
}
