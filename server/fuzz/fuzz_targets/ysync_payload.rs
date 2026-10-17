#![no_main]
//! Raw WebSocket frames from clients go through `handle_payload`: y-sync messages (updates,
//! state vectors, awareness) decoded by yrs. Garbage must be an `Err`, never a panic.
use std::collections::HashSet;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use samsyn_server::limits::RequestBudget;
use samsyn_server::ws::{handle_payload, PayloadLimits};
use yrs::sync::{Awareness, DefaultProtocol};
use yrs::Doc;

fuzz_target!(|data: &[u8]| {
    let mut aw = Awareness::new(Doc::new());
    let mut owned = HashSet::new();
    let mut step1 = RequestBudget::new(1000, Duration::from_secs(60));
    let mut limits = PayloadLimits { doc_bytes: (0, 0), step1: &mut step1 };
    let _ = handle_payload(&DefaultProtocol, &mut aw, data, &mut owned, true, &mut limits);
    let mut limits = PayloadLimits { doc_bytes: (0, 64), step1: &mut step1 };
    let _ = handle_payload(&DefaultProtocol, &mut aw, data, &mut owned, false, &mut limits);
});
