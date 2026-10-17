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

/// A content hash as used for blobs: exactly 64 hex characters. Anything else never reaches
/// the filesystem (blob metadata comes from other people's documents).
pub fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Is this a document key (vault-relative file path) that clients may safely map to a file?
/// Keys come from other users (shares), so the rules are strict: relative, "/"-separated,
/// no empty, "." or ".." segments, no control characters, no backslashes, bounded length.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 1024
        && !key.starts_with('/')
        && !key.ends_with('/')
        && !key.contains('\\')
        && !key.bytes().any(|b| b < 0x20 || b == 0x7f)
        && key.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod key_tests {
    use super::valid_key;

    #[test]
    fn accepts_normal_paths() {
        for k in ["a.md", "Projects/X/a.md", "deep/er/path/with space.png", ".obsidian/plugins/x/data.json"] {
            assert!(valid_key(k), "{k}");
        }
    }

    #[test]
    fn rejects_escapes() {
        for k in ["", "/etc/passwd", "../x.md", "a/../../x.md", "a/./b", "a//b", "a/", "a\\b", "a\u{0}b", "a\nb"] {
            assert!(!valid_key(k), "{k:?}");
        }
        assert!(!valid_key(&"a".repeat(1025)));
    }
}
