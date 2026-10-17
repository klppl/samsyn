#![no_main]
//! Tickets are pasted by users and decoded by servers and plugins. Decoding must never panic.
use libfuzzer_sys::fuzz_target;
use samsyn_server::ticket::Ticket;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = Ticket::decode(&s);
    // Most random input fails the prefix check; also try it as the payload after the prefix.
    let _ = Ticket::decode(&format!("samsyn{s}"));
});
