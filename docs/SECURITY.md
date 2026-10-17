# Security process

How security is handled in this repo, so it stays handled when features move fast. The threat model and the public-exposure guide are in [PUBLIC-SERVER.md](PUBLIC-SERVER.md); this file is about *process*.

## Attackers we design against

| Who | Has | Must not get |
|---|---|---|
| Anonymous on the internet | the server URL, maybe a node id | anything but `/health`; a way to lock out real users |
| Holder of one share ticket | the ticket secret (or a session) | anything outside that share; write access on a read-only ticket; a way to enumerate shares |
| Another user on the same server | their own token | another user's vaults, blobs, shares, mounts, tokens |
| A collaborator on a shared folder | write access to document keys and content | a file written outside the share folder on anyone's disk; anything outside the share |
| A peer server on the p2p protocol | the node id, a connection | share data without a valid secret; server resources without bound |
| Someone holding a backup or the data dir | files at rest | nothing we can do beyond documenting what is sensitive (`registry.db`, mount secrets) |

## Security regression tests

`server/tests/security.rs` runs with plain `cargo test` and in CI. Every test pins a property an attacker would try to break; a failing test is a security regression, never "flaky". Groups:

- **anonymous**: every authenticated route answers 401 to no/bad tokens; nothing sensitive on public routes; tokens and secrets never appear in the server log, including via query strings; malformed names, hashes, paths, tickets and oversized bodies are rejected; garbage and oversized WebSocket frames do not kill the server.
- **wrong user**: same vault name resolves per user; blobs by hash are per vault; share, ticket, mount and token management checks ownership; a mount exposes only the shared subtree.
- **ticket holder**: unknown share and wrong secret are indistinguishable; the rate limiter cannot be dodged with a forged `X-Forwarded-For`; read-only tickets cannot upload; a guest session is not a user token and dies with its ticket.
- **collaborator**: hostile document keys (`/abs`, `..`, `\`, empty segments, control characters) injected straight over the WebSocket reach no disk anywhere and are not even bridged into the other vault.

Adding a finding → add a test here first that fails, then fix. Name the test after the property, not the bug.

## Checks in CI

- `cargo test` (unit, scenario and security tests), `cargo audit` (RustSec advisories; unmaintained-only warnings do not fail), `npm audit --omit=dev --audit-level=high` for the plugin bundle.
- Lockfiles are committed (`Cargo.lock`, `package-lock.json`); CI installs from them (`npm ci`).

## Rules of thumb in the code

- Every document key that is about to become a file path goes through `valid_key` (server `ids.rs`, bridges) / `isSafeKey` (plugin) / `should_sync` (headless). Never join a key onto a directory without it.
- New routes: authenticate first, scope to the caller (`user_vault`, `owned_share`, mount owner), validate ids with `valid_name` / `valid_hash`, bound body size, log the path only.
- Anything a stranger can call before authentication must be cheap and bounded (hello frames, hash frames, JSON bodies) and must answer identically for "does not exist" and "wrong secret".
- Secrets: store hashed when we only need to verify (user tokens, ticket secrets on the owner side); when we must present them later (mount secrets, guest mounts in the plugin) say so in STORAGE.md and treat the file as sensitive. Print a secret at most once, where the operator asked for it.
- No `unsafe` in this crate. Network input is decoded by `yrs`, `postcard`, `serde_json` and `tungstenite`, all with size limits in front of them (`MAX_WS_MESSAGE`, frame caps in `remote.rs`, `DefaultBodyLimit`, axum's JSON limit).

## Audit log

| Date | Scope | Result |
|---|---|---|
| 2026-10-17 | First audit: dependencies, secrets, auth design, memory safety, all public routes, p2p, clients | Findings and plan in the project vault (Replica/Security); fixes in the same commit as the test suite |
