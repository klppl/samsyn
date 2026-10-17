#![no_main]
//! Everything that becomes a path goes through these; they must never panic and must never
//! accept something that escapes a directory.
use libfuzzer_sys::fuzz_target;
use replica_server::ids::{valid_hash, valid_key};

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    if valid_key(&s) {
        assert!(!s.starts_with('/'));
        assert!(!s.split('/').any(|seg| seg.is_empty() || seg == "." || seg == ".."));
        assert!(!s.contains('\\'));
        assert!(!s.bytes().any(|b| b < 0x20 || b == 0x7f));
    }
    if valid_hash(&s) {
        assert_eq!(s.len(), 64);
        assert!(s.bytes().all(|b| b.is_ascii_hexdigit()));
    }
});
