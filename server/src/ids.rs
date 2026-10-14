use rand::RngCore;

/// 16 hex chars, random. Used for user ids, vault ids and the instance id.
pub fn new_id() -> String {
    let mut b = [0u8; 8];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Opaque bearer token handed to a device: `rp_` + 32 hex chars.
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    format!("rp_{}", hex::encode(b))
}

pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.trim().as_bytes()))
}

pub fn valid_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !id.starts_with('.')
}
