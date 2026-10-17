#![no_main]
//! Deltas recorded on one document are replayed on another. Positions are attacker-influenced
//! (a collaborator controls the source document); replay must never panic inside yrs.
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use replica_server::bridge::{replay_text_delta, OwnedDelta};
use yrs::{Doc, GetString, Text, Transact};

#[derive(Arbitrary, Debug)]
struct Case {
    start: String,
    full: String,
    ops: Vec<(u8, u16, String)>,
}

fuzz_target!(|case: Case| {
    let doc = Doc::new();
    let t = doc.get_or_insert_text("t");
    let delta: Vec<OwnedDelta> = case
        .ops
        .iter()
        .take(64)
        .map(|(k, n, s)| match k % 3 {
            0 => OwnedDelta::Retain(*n as u32),
            1 => OwnedDelta::Delete(*n as u32),
            _ => OwnedDelta::Insert(s.chars().take(64).collect()),
        })
        .collect();
    let mut txn = doc.transact_mut();
    t.insert(&mut txn, 0, &case.start);
    let fit = replay_text_delta(&mut txn, &t, &delta, &case.full);
    if !fit {
        // Fallback must leave the text equal to `full`.
        assert_eq!(t.get_string(&txn), case.full);
    }
});
