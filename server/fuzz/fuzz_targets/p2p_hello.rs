#![no_main]
//! The first frames on a p2p connection are decoded before authentication.
use libfuzzer_sys::fuzz_target;
use samsyn_server::remote::{Hello, HelloReply};

fuzz_target!(|data: &[u8]| {
    let _ = postcard::from_bytes::<Hello>(data);
    let _ = postcard::from_bytes::<HelloReply>(data);
});
