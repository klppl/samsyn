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

Scenario test (one server, three users, headless clients as plugins): `share-e2e.sh` in the session scratchpad; a copy belongs in `server/tests/` once it is turned into a Rust integration test.

## Tests

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
