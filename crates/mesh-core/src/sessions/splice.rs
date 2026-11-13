//! The origin end of a session (M17 §3.3).
//!
//! The origin daemon sits between a local client and a remote host and is
//! deliberately **a splice, not a translator**: the same two frame enums
//! cross both legs, so it copies frames and counts bytes and does not
//! interpret a single one. A resize means what the host says it means,
//! because nothing in the middle ever had an opinion.
//!
//! That is not just tidiness. The alternative — an origin that understands
//! sessions — is an origin that has to be upgraded in lockstep with both
//! ends, and a third place for the meaning of `Eof` to drift.

use tokio::io::{AsyncRead, AsyncWrite};

use mesh_proto::{SessionFrame, SessionServerFrame, read_frame, write_frame};

use super::Guard;

/// Copy frames in both directions until either end stops.
///
/// Returns when the *host* leg ends, which is the one that carries the
/// verdict: a client that walks away should not make the origin forget to
/// tell the host, and a host that exits ends the session for the client
/// whatever the client is doing.
pub async fn run<CR, CW, HR, HW>(
    client_read: CR,
    client_write: CW,
    host_read: HR,
    host_write: HW,
    guard: &Guard,
) where
    CR: AsyncRead + Unpin + Send + 'static,
    CW: AsyncWrite + Unpin + Send + 'static,
    HR: AsyncRead + Unpin + Send + 'static,
    HW: AsyncWrite + Unpin + Send + 'static,
{
    let mut up = tokio::spawn(upstream(client_read, host_write));
    let mut down = tokio::spawn(downstream(host_read, client_write));

    tokio::select! {
        bytes = &mut down => {
            guard.count_out(bytes.unwrap_or(0));
            up.abort();
        }
        bytes = &mut up => {
            guard.count_in(bytes.unwrap_or(0));
            // The client hung up. Give the host a moment to answer — a
            // `cp` that has sent its last byte and its digest is about to
            // hear "exit 0", and dropping the stream here would turn a
            // finished copy into a failed one.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut down).await;
            down.abort();
        }
        _end = guard.closed() => {
            up.abort();
            down.abort();
        }
    }
}

async fn upstream<CR, HW>(mut client_read: CR, mut host_write: HW) -> usize
where
    CR: AsyncRead + Unpin,
    HW: AsyncWrite + Unpin,
{
    let mut total = 0usize;
    loop {
        let frame: SessionFrame = match read_frame(&mut client_read).await {
            Ok(f) => f,
            Err(_) => return total,
        };
        if let SessionFrame::Data(bytes) = &frame {
            total += bytes.len();
        }
        if write_frame(&mut host_write, &frame).await.is_err() {
            return total;
        }
    }
}

async fn downstream<HR, CW>(mut host_read: HR, mut client_write: CW) -> usize
where
    HR: AsyncRead + Unpin,
    CW: AsyncWrite + Unpin,
{
    let mut total = 0usize;
    loop {
        let frame: SessionServerFrame = match read_frame(&mut host_read).await {
            Ok(f) => f,
            Err(_) => return total,
        };
        if let SessionServerFrame::Data { bytes, .. } = &frame {
            total += bytes.len();
        }
        let terminal = matches!(
            frame,
            SessionServerFrame::Exit { .. } | SessionServerFrame::Error { .. }
        );
        if write_frame(&mut client_write, &frame).await.is_err() {
            return total;
        }
        if terminal {
            return total;
        }
    }
}
