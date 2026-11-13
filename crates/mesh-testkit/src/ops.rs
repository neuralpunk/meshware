//! Crafting ops by hand, including ops this build could not author.
//!
//! Two audiences. Tests that need a *hostile* op — a valid signature from
//! the wrong author, a tampered body — build it with [`seal`]. Tests that
//! need an op from a **future protocol version** build it with
//! [`future_body`] + [`seal_raw`]: there is no `OpBody` variant to
//! construct, so the body is assembled as postcard bytes directly. That is
//! the only way to exercise F0's carry-what-you-cannot-read path before a
//! milestone actually adds an entity kind.

use mesh_core::identity::Keypair;
use mesh_proto::{Hlc, OpBody, OpEnvelope, op_signing_bytes_raw};

/// A wall-clock HLC for a hand-built op.
pub fn now_hlc() -> Hlc {
    Hlc {
        wall_micros: mesh_proto::now_micros(),
        logical: 0,
    }
}

/// Sign an op body as `key`, producing the envelope a peer would receive.
pub fn seal(key: &Keypair, seq: u64, hlc: Hlc, body: &OpBody) -> OpEnvelope {
    seal_raw(
        key,
        seq,
        hlc,
        postcard::to_stdvec(body).expect("encode body"),
    )
}

/// Sign *raw* body bytes as `key`. The bytes are never decoded here, so
/// they may be anything — including a variant index no build understands.
pub fn seal_raw(key: &Keypair, seq: u64, hlc: Hlc, body: Vec<u8>) -> OpEnvelope {
    let author = key.node_id();
    let signature = key
        .sign(&op_signing_bytes_raw(&author, seq, &hlc, &body))
        .to_vec();
    OpEnvelope {
        author,
        seq,
        hlc,
        body,
        signature,
    }
}

/// An `OpBody` encoding for a variant index this build has no code for —
/// what an op authored by a newer meshnode looks like on the wire.
///
/// postcard puts the variant index first as a LEB128 `u32`; `payload` is
/// whatever that variant's fields would have encoded to. Nothing validates
/// it, which is the point: the receiving node must handle a body it cannot
/// parse, not merely one it can parse and dislikes.
pub fn future_body(index: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = varint(index);
    out.extend_from_slice(payload);
    out
}

/// postcard's LEB128 encoding of a `u32`.
fn varint(mut n: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_matches_postcard() {
        // postcard encodes an enum discriminant with the same varint, so a
        // round trip through a real `OpBody` pins the encoding.
        let body = postcard::to_stdvec(&OpBody::MemberLeft {
            node_id: mesh_proto::NodeId([1u8; 32]),
        })
        .expect("encode");
        assert_eq!(body[0..1], varint(3)[..], "MemberLeft is index 3");
        assert_eq!(varint(0), vec![0x00]);
        assert_eq!(varint(27), vec![0x1b]);
        assert_eq!(varint(127), vec![0x7f]);
        assert_eq!(varint(128), vec![0x80, 0x01]);
        assert_eq!(varint(300), vec![0xac, 0x02]);
    }

    #[test]
    fn future_bodies_do_not_decode() {
        let body = future_body(99, b"whatever a 1.9 node put here");
        assert!(postcard::from_bytes::<OpBody>(&body).is_err());
        assert_eq!(mesh_proto::op_body_index(&body), Some(99));
    }
}
