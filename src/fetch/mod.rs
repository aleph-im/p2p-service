// NOTE: requester lands in a later task; the declaration is commented out
// so the crate compiles at every step.
pub mod provider;
// pub mod requester;
pub mod wire;

use libp2p::StreamProtocol;

pub const FETCH_PROTOCOL: StreamProtocol = StreamProtocol::new("/aleph/fetch/1.0.0");

/// Hashes are sha256 hex digests or base58/base32 CIDs; this guard also
/// prevents path traversal when the hash is interpolated into the provider URL.
pub fn is_valid_item_hash(hash: &str) -> bool {
    !hash.is_empty() && hash.len() <= 128 && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}
