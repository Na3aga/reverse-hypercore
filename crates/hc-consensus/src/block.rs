//! Block envelope and wire messages.
//!
//! Simplex only orders 32-byte digests. The digest it orders is the SHA-256 of
//! a small *envelope* that wraps the application's opaque payload together
//! with the consensus context it was built in (view, parent, proposer). Binding
//! the context into the digest means a payload cannot be replayed into a
//! different view or onto a different parent, and lets every validator
//! reconstruct the finalized chain by walking parent links.

use bytes::Bytes;
use commonware_cryptography::{sha256::Digest, Hasher as _, Sha256};

/// Maximum size of an application payload, in bytes.
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Size of the fixed envelope header:
/// `view (8) | parent_view (8) | parent (32) | proposer (4) | payload_len (4)`.
const HEADER_LEN: usize = 8 + 8 + 32 + 4 + 4;

/// Digest of the genesis "block" (all zeros).
pub(crate) const GENESIS: Digest = Digest([0u8; 32]);

/// A decoded block envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Block {
    pub view: u64,
    pub parent_view: u64,
    pub parent: [u8; 32],
    pub proposer: u32,
    pub payload: Vec<u8>,
}

impl Block {
    /// Canonical big-endian encoding.
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.view.to_be_bytes());
        out.extend_from_slice(&self.parent_view.to_be_bytes());
        out.extend_from_slice(&self.parent);
        out.extend_from_slice(&self.proposer.to_be_bytes());
        let len = u32::try_from(self.payload.len()).expect("payload length checked by caller");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&self.payload);
        Bytes::from(out)
    }

    /// Strict decoding: rejects trailing bytes and oversized payloads, so the
    /// encoding is canonical (`encode(decode(b)) == b`).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let (header, payload) = bytes.split_at(HEADER_LEN);
        let u64_at = |i: usize| u64::from_be_bytes(header[i..i + 8].try_into().unwrap());
        let u32_at = |i: usize| u32::from_be_bytes(header[i..i + 4].try_into().unwrap());
        let len = u32_at(52) as usize;
        if len != payload.len() || len > MAX_PAYLOAD_BYTES {
            return None;
        }
        Some(Self {
            view: u64_at(0),
            parent_view: u64_at(8),
            parent: header[16..48].try_into().unwrap(),
            proposer: u32_at(48),
            payload: payload.to_vec(),
        })
    }
}

/// SHA-256 of the encoded envelope: the digest simplex orders.
pub(crate) fn digest(encoded: &[u8]) -> Digest {
    Sha256::hash(&[encoded])
}

/// Messages exchanged on the dedicated block-dissemination channel.
pub(crate) enum Wire {
    /// Full encoded envelope (self-authenticating via its digest).
    Block(Bytes),
    /// Request for the envelope with the given digest.
    Request(Digest),
}

const TAG_BLOCK: u8 = 0;
const TAG_REQUEST: u8 = 1;

impl Wire {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Block(bytes) => {
                let mut out = Vec::with_capacity(1 + bytes.len());
                out.push(TAG_BLOCK);
                out.extend_from_slice(bytes);
                out
            }
            Self::Request(d) => {
                let mut out = Vec::with_capacity(33);
                out.push(TAG_REQUEST);
                out.extend_from_slice(&d.0);
                out
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&tag, rest) = bytes.split_first()?;
        match tag {
            TAG_BLOCK => Some(Self::Block(Bytes::copy_from_slice(rest))),
            TAG_REQUEST => Some(Self::Request(Digest(rest.try_into().ok()?))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let b = Block {
            view: 7,
            parent_view: 5,
            parent: [9u8; 32],
            proposer: 2,
            payload: b"hello".to_vec(),
        };
        let enc = b.encode();
        assert_eq!(Block::decode(&enc), Some(b));
        let mut bad = enc.to_vec();
        bad.push(0);
        assert_eq!(Block::decode(&bad), None);
    }
}
