# Development

## Prerequisites

- Node 20+ and npm
- Rust stable (1.80+) with cargo
- Obsidian desktop, and a phone with Obsidian mobile for mobile testing

## Server

```sh
cd server
cargo run -- serve --bind 127.0.0.1:8787 --data-dir ./data
```

Useful during development:

```sh
RUST_LOG=debug cargo run -- serve          # verbose logs
cargo run -- user add test                  # extra user + token
cargo run -- vault list                     # what is stored
cargo test                                  # unit tests
```

The data dir is throwaway; delete `./data` to start from scratch (a new owner token is printed).

## Plugin on desktop

Obsidian loads plugins from `<vault>/.obsidian/plugins/<id>/` and needs three files there: `main.js`, `manifest.json`, `styles.css`.

1. Create a scratch vault for development, for example `~/ObsidianDev`.
2. Point the plugin's build output at it. A symlink is the easiest:

   ```sh
   cd plugin
   npm install
   ln -s "$(pwd)" ~/ObsidianDev/.obsidian/plugins/replica
   ```

   On Windows use a junction: `mklink /J C:\path\to\vault\.obsidian\plugins\replica C:\path\to\replica\plugin`.

3. Start the watcher:

   ```sh
   npm run dev
   ```

   This rebuilds `main.js` on every save (with inline source maps).

4. In Obsidian: Settings → Community plugins → turn off Restricted mode → enable **Replica**.

5. After each rebuild, reload the plugin. Either toggle it off/on in settings, or install the community plugin **Hot Reload** (pjeby/hot-reload), which reloads any plugin whose folder contains a `.hotreload` file:

   ```sh
   touch plugin/.hotreload
   ```

6. Open the developer console with `Ctrl/Cmd+Shift+I`. Replica logs with the `[replica]` prefix. To inspect the shared document from the console:

   ```js
   const p = app.plugins.plugins.replica;
   p.engine.texts.toJSON();           // every synced text file
   p.engine.blobs.toJSON();           // every synced binary
   ```

### Two devices on one machine

Open a second vault (File → Open another vault) with the same Server URL, token and **Vault ID**. Each Obsidian window is a separate client, so you can watch live sync between two windows side by side.

### Emulating mobile on desktop

Most mobile behaviour (layout, `Platform.isMobile`) can be checked on desktop. In the console:

```js
app.emulateMobile(true);   // reload later with app.emulateMobile(false)
```

This does not emulate the mobile WebView, so always test the real thing before a release.

## Plugin on a phone

Obsidian mobile loads plugins from the same `.obsidian/plugins/replica/` folder inside the vault, but there is no symlink or watcher. You copy the built files in. Build first:

```sh
cd plugin
npm run build      # production main.js, minified, no source map
```

Then get `main.js`, `manifest.json` and `styles.css` into `<vault>/.obsidian/plugins/replica/` on the phone.

### iOS

The Files app hides folders that start with a dot, so you cannot browse into `.obsidian` from the phone itself. Pick one of these:

**A. iCloud vault, copy from a Mac (easiest).** If the mobile vault is stored in iCloud Drive, it is also on your Mac at:

```
~/Library/Mobile Documents/iCloud~md~obsidian/Documents/<VaultName>/
```

Copy the files there and iCloud pushes them to the phone:

```sh
V=~/Library/Mobile\ Documents/iCloud~md~obsidian/Documents/MyVault
mkdir -p "$V/.obsidian/plugins/replica"
cp plugin/main.js plugin/manifest.json plugin/styles.css "$V/.obsidian/plugins/replica/"
```

Wait for the iCloud upload (Finder shows a cloud icon until done), then on the phone: Settings → Community plugins → reload (pull down or toggle the plugin). The first time, enable it from the list.

**B. On-device vault, copy via Finder/USB.** Connect the phone, open Finder → iPhone → Files → Obsidian → drag the `replica` folder into the vault's `.obsidian/plugins/` folder. Finder does show dot-folders here. (On older macOS this is in iTunes → File Sharing.)

**C. BRAT (once we publish pre-releases).** Install the BRAT community plugin on the phone, add this repository as a beta plugin, and it downloads `main.js`/`manifest.json`/`styles.css` from GitHub releases. This is the plan for testers before the community-list release.

**D. Replica itself.** Once one device is syncing, you can tick "sync the `.obsidian` folder" in a later version and let Replica carry the plugin to the other devices. Not implemented yet, and risky for plugin files; listed for completeness.

### Android

Same three files into `<vault>/.obsidian/plugins/replica/`. Any file manager that shows hidden files works (Material Files, Solid Explorer, or the built-in Files app with "Show hidden files"), or from a computer:

```sh
adb push plugin/main.js plugin/manifest.json plugin/styles.css \
  /sdcard/Documents/MyVault/.obsidian/plugins/replica/
```

### Debugging on a phone

Obsidian mobile does not expose a developer console. Options, in order of usefulness:

0. **Settings → Replica → Status.** Live status, whether the initial sync completed, peer count, file counts, the exact WebSocket URL, last error and a hint. Also the command **Replica: Show sync status**. Check this first.
1. **Watch the server.** `RUST_LOG=debug cargo run -- serve` shows every connection, auth failure and message. Most mobile issues (wrong URL, token, mixed content) are visible there.
2. **Use the status bar and Notices.** Replica reports connection state in the status bar and shows a Notice on errors that matter.
3. **Android: Chrome remote debugging.** With USB debugging enabled, `chrome://inspect` on a desktop Chrome lists the Obsidian WebView and gives a full console. Obsidian on Android must be a debuggable build for this; recent releases are, if not, fall back to 1 and 2.
4. **iOS: Safari Web Inspector** only works for apps built with a developer profile, which the App Store Obsidian is not. Use 1 and 2.

### Mobile gotchas

- **iOS needs `https://`.** The WebView is a secure context, so `new WebSocket("ws://…")` is refused as insecure mixed content, while `Test connection` still passes because `requestUrl` goes through native code. Symptoms: test OK, status stays "Offline" or "Connecting", hint on the settings page mentions ws://. Fix: Tailscale serve, Caddy, or `--tls-cert/--tls-key` (see README). Android is more lenient but treat it the same.
- Self-signed certificates: install the CA on the phone and enable full trust for it (iOS: Settings → General → About → Certificate Trust Settings). `mkcert -install` plus AirDropping `rootCA.pem` is the quickest dev setup.
- iOS suspends the app in the background and closes the WebSocket. Replica reconnects when the app returns and the reconcile step catches up on anything missed.
- `WebSocket` headers cannot be set on mobile, which is why the token also goes in the URL query string.

## Engines and scopes

`SyncEngine` is instantiated once per sync target (`EngineTarget`): the *home* engine (own server, whole vault, keys == vault paths) and one *guest* engine per guest mount (owner's server, one folder, keys relative to the folder). All path mapping happens at the Obsidian boundary: `keyOf(vaultPath)` returns the document key or `null` when the path is outside this engine's scope (other engines' roots, config dir, excludes); `pathOf(key)` goes back. Everything between (observers, reconcile, conflict copies, mass-delete guard) works on keys and is identical for both kinds. `ReplicaPlugin.engines` holds them; `plugin.engine` is the home engine.

## Local cache on the device

The plugin keeps two things per device and vault, both in **IndexedDB** (`src/persistence.ts`), never in the vault folder:

- the Yjs document, via `y-indexeddb`: an append-only log of updates, compacted into one record every 500 entries. A keystroke costs one small write; nothing rewrites the whole document. Database `replica-doc-<installId>|<vaultId>`.
- the per-path "last synced" hashes, one record per path, flushed in batches about a second after a change. Database `replica-state-…`.

Why not files in `.obsidian/plugins/replica/`: the previous `doc.bin`/`state.json` were rewritten in full on every change (a 50 MB vault meant 50 MB writes while typing), and a copied or iCloud-synced `.obsidian` carried one device's bookkeeping to another, which is how a phone once deleted a vault. Old files are migrated into IndexedDB on first start and removed. Without IndexedDB (not a case in Obsidian today) the file backend is still there as a fallback.

"Reset local state" deletes both databases. The Status section shows the backend, the encoded document size and the browser's storage usage/quota.

### Size limits, and what actually bounds a vault on a phone

APFS has no 4 GB file limit (that is FAT32); IndexedDB stores are split into many records anyway. What bounds a vault on iOS is **RAM**: Yjs keeps the whole document in memory, and iOS kills an app that goes past about 1–1.5 GB. Measured with `yjs` 13.6 in Node (heap growth per byte of text):

| How the text got into the document | Heap per byte of text |
|---|---|
| Inserted whole (first sync, files written outside the editor) | ≈ 1× |
| Typed live, word by word, in the current session | ≈ 10× |
| Typed live, character by character, in the current session | ≈ 32× |
| Any of the above after a reload (items merge on load) | ≈ 1× |
| 20 % of the text deleted (tombstones remain) | ≈ 2× |

So at startup a vault costs about its text size, plus tombstones; live typing only inflates what was typed since the last start. Loading a 50 MB document takes ~20 ms and encoding it ~40 ms (what `y-indexeddb` does every 500 updates). Rules of thumb for a phone: up to ~200 MB of Markdown is fine, 200–500 MB works with slow first sync, beyond that the single-document model needs the partial-loading design below. Binary files never enter the document (only `{hash, size, mtime}`), so attachments do not count.

What still costs at the high end: the first sync sends the whole state as one WebSocket message (a buffer the size of the vault, decoded on the phone), and the server holds every loaded vault in RAM too. The Status section warns above 100 MB of document. If a user hits the limit the fix is per-file documents loaded on demand behind a small index document (Yjs subdocs over a multiplexed stream); it is a protocol change on both sides and is not started.

## Safety behaviour worth knowing while testing

- **Sync state is device-bound.** `state.json` records which files this install has seen in sync. It is tagged with a per-device install id (Obsidian's `appId`, falling back to localStorage) and ignored if it was copied from another device along with `.obsidian`. Copying `doc.bin` is harmless; copying `state.json` used to make the new device think every file had been deleted locally.
- **Mass-delete guard.** If one reconcile would remove at least 5 files and more than half of the shared vault, the plugin refuses, re-downloads them, shows a Notice, and lists them under *Held back* on the settings page. The command **Replica: Apply held-back deletions** does it for real. Single and small deletions are applied normally.
- **Remote deletions go to trash**, never a hard delete (Obsidian's "Deleted files" setting decides system trash vs `.trash/`).

## Headless client

`replica-server sync` lives in `server/src/client/`. It reuses the server's yrs dependency and speaks the same y-sync protocol as a client. Structure:

- `mod.rs` — flags, URL building, the main `select!` loop (WebSocket events, watcher batches, 2 s housekeeping tick, `--once` exit).
- `ws.rs` — connection task with exponential reconnect; hands the engine a per-connection sender.
- `engine.rs` — owns the `Doc`/`Awareness`, `texts` and `blobs` maps, reconcile, local → doc pushes, doc → disk writes, three-way merge for pending local edits, mass-delete guard.
- `diff.rs` — byte-offset single-span diff (yrs text indexes are UTF-8 bytes by default).
- `state.rs` — `<dir>/.replica/{state.json,doc.bin,trash/}`; install id derived from hostname + absolute path.

Quick manual test with two directories:

```sh
cargo run -- serve --data-dir /tmp/rd --bind 127.0.0.1:8787    # note the token
mkdir -p /tmp/a /tmp/b
cargo run -- sync --server http://127.0.0.1:8787 --token rp_… --vault t --dir /tmp/a --device A
cargo run -- sync --server http://127.0.0.1:8787 --token rp_… --vault t --dir /tmp/b --device B
echo hi > /tmp/a/x.md && sleep 1 && cat /tmp/b/x.md
```

Leftover `sync` processes keep watching their directory. Kill them before deleting a test directory, or the guard will (rightly) hold back the deletions and log about it.

## Sharing (server-side bridges)

`server/src/bridge.rs` and the `DocRegistry` in `server/src/vault.rs`. Every CRDT apply on the server goes through `DocRegistry::apply_lock`; after an apply the caller runs `propagate`, which drains the changes recorded by observers and replays them across bridges. Rules that bit during development:

- Never call `doc.get_or_insert_map` while a `transact_mut` on the same doc is alive (self-deadlock with the `sync` feature); use `txn.get_or_insert_map`.
- Do not hold a `vaults`/`shares` map guard across a call that locks it again (`if let Some(x) = self.map.lock()...` keeps the guard for the whole `if let`).
- All vaults that take part in a bridge are loaded at startup (`preload_bridged`), otherwise a share can advance while one end is unloaded and the later reconcile overwrites it.

Scenario test (one server, three users, headless clients as plugins): `server/scripts/share-e2e.sh`. Needs a debug build (`cargo build` in `server/`), uses port 8798 and a temp dir (`REPLICA_E2E_DIR=… ` to keep it), kills every running `replica-server` first. Covers create, overlap race, mount, both-way and concurrent edits, blobs, read-only revert and rescue copy, revoke, restart, unmount. Turning it into a Rust integration test is still on the list.

Guest mode: `server/scripts/guest-e2e.sh` (owner server with `--public-url`, two headless guests via `--ticket`, rw and ro, auth hygiene, revoke). Presence across a bridge: `server/scripts/presence-e2e.sh` (one server, Node y-websocket clients from `plugin/tests/presence-e2e.mjs`, needs `npm install` in `plugin/`).

### Server to server (iroh)

`server/src/remote.rs`: endpoint, accept handler (`ShareProtocol`), link task per remote mount (`spawn_link`), framing and blob fetch. Things worth knowing:

- Both servers on one machine need different `--p2p-port`s. `--relay none` makes the test deterministic (direct connection over loopback/LAN, no internet); without it the endpoint registers with the public n0 relays, which takes a few seconds and is done in the background so HTTP is not delayed.
- A ticket embeds the owner's direct addresses *with port*, so a random p2p port (`--p2p-port 0`) breaks old tickets after a restart unless a relay is configured. Hence the fixed default.
- Blob bytes cross the link lazily: the metadata can reach a vault before the bytes do. Both clients retry a 404 with backoff; the server copies fetched bytes into the bridged vault store (`DocRegistry::blob_arrived`).
- When the owner denies a hello it waits for the reply to drain before closing, otherwise the peer only sees "connection lost" and keeps retrying instead of marking the mount revoked.

Scenario test: `server/scripts/remote-e2e.sh` (two servers, ports 8798/8799 + p2p 9798/9799, `REPLICA_E2E_RELAY=none|default|<url>`). Covers probe of a bad ticket, mount over p2p, blobs both ways, concurrent edits, read-only, revoke → mount revoked on the other server, restart of either server, unmount. It takes 3–4 minutes.

## Changing the registry schema

Numbered migrations in `server/src/storage/registry.rs` (`migrate()`, `REGISTRY_SCHEMA_VERSION`, `PRAGMA user_version`). Add a step, bump the version, add a `migration_tests` case that opens the old shape. `CREATE TABLE IF NOT EXISTS` never changes an existing table, which is exactly how the first scaffold's reserved `mounts` table survived into a running server and broke it. See docs/STORAGE.md "Schema migrations".

## Tests

Sharing scenarios also exist as Rust integration tests in `server/tests/` (`sharing_same_server`, `sharing_remote`, `sharing_guest`; helpers in `tests/common/mod.rs`). They spawn the freshly built binary as servers and headless clients in a temp dir and take 1–3 minutes each:

```sh
cd server && cargo test --test sharing_same_server   # or --test sharing_remote / --test sharing_guest
```

They run as part of plain `cargo test` too. The zsh scripts under `server/scripts/` remain for poking at a scenario by hand.

```sh
cd plugin && npm test                       # diff/hash unit tests (node:test)
cd plugin && node tests/e2e.mjs ws://127.0.0.1:8787/ws rp_yourtoken   # against a running server
cd server && cargo test
```

## Release checklist (later)

1. Bump `version` in `plugin/manifest.json`, `plugin/package.json`, `plugin/versions.json`.
2. `npm run build`, attach `main.js`, `manifest.json`, `styles.css` to a GitHub release tagged with the version.
3. Submit to the Obsidian community plugin list following the official guidelines (`isDesktopOnly: false`, no `innerHTML`, etc.).
4. Build server binaries (`cargo build --release`) for Linux x86_64/arm64, macOS, Windows; publish the Docker image.
