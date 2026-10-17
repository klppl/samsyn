# Vendored yrs 0.28.0 — local security patch

This is an unmodified copy of `yrs` 0.28.0 (from crates.io) with one security fix applied,
pulled in via `[patch.crates-io]` in `server/Cargo.toml` and `server/fuzz/Cargo.toml`.

## The change

CRDT decoders (`Update`, `IdSet`/`IdRanges`, `StateVector`) read an element count as a varint
and then reserve/loop that many times. `try_reserve` was already used, but it only turns an
*allocator failure* into an error; under memory overcommit (Linux/macOS default) the kernel
honours a multi-gigabyte reservation and the process is OOM-killed when the pages are touched.
A ~18-byte crafted sync message could make the server attempt tens of GB.

Fix: `Decoder` gains `remaining()` (bytes left to read; `Some` for the v1 byte decoder, `None`
otherwise) and `check_len(len)`, which rejects a declared count larger than the bytes still
available — every element costs at least one wire byte, so a larger count is always malformed.
`check_len` is called before each reserve/loop in `update.rs`, `id_set.rs` and `state_vector.rs`.
v2 decoders return `None` (counts there are spread across sub-streams), so their behaviour is
unchanged; this crate only ever puts v1 on the wire.

Diff is limited to:
- `src/updates/decoder.rs` — `remaining()` + `check_len()` on the trait, `remaining()` for `DecoderV1`.
- `src/update.rs`, `src/id_set.rs`, `src/state_vector.rs` — a `check_len(...)?` before each count use.

## Why vendored, not upstreamed yet

We want to run this in production for a while and confirm it rejects nothing legitimate before
filing upstream. The upstream report is tracked as a private issue (base for a yrs issue + PR later). When
upstream ships the fix, drop `third_party/yrs` and the two `[patch]` stanzas and bump the `yrs`
version.
