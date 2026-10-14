# Replica

Conflict-free live sync for [Obsidian](https://obsidian.md), self-hosted.

Replica is two parts:

- **`plugin/`** — an Obsidian plugin (TypeScript, no native code, runs on desktop, iOS and Android).
- **`server/`** — a small Rust server that stores each vault as a CRDT document and relays changes between devices in real time.

Text files (`.md`, `.txt`, …) are merged character by character with [Yjs](https://github.com/yjs/yjs) on the plugin side and [yrs](https://github.com/y-crdt/y-crdt) on the server, so two devices editing the same note at the same time converge without a conflict. Binary files (images, PDFs) are synced whole, last writer wins. Everything works offline and catches up when you reconnect.

> Status: early development. Not yet published to the Obsidian community plugin list.

## Quick start

### 1. Run the server

```sh
cd server
cargo run --release -- serve
```

On first start it creates a user named `owner` and prints a token once:

```
INFO first start: created user 'owner'
INFO   token: rp_...
```

Data lives in `./data` by default (`--data-dir` or `REPLICA_DATA_DIR` to change). See [docs/STORAGE.md](docs/STORAGE.md) for the on-disk layout, backups and snapshots.

Or with Docker:

```sh
docker compose up -d
docker compose logs server | grep token
```

### 2. Install the plugin

Until it is in the community list, install it manually or with [BRAT](https://github.com/TfTHacker/obsidian42-brat). See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for desktop and phone instructions.

### 3. Connect

Obsidian → Settings → Replica:

| Setting    | Value                                                        |
|------------|--------------------------------------------------------------|
| Server URL | `http://your-host:8787` (or `https://…` behind a reverse proxy) |
| Token      | the `rp_…` token printed by the server                       |
| Vault ID   | defaults to the vault name; use the same ID on every device  |

Press **Test connection**, then **Reconnect**. The status bar shows `Replica` when live, and how many other devices are on the same vault.

Repeat on each device with the same server, token and vault ID. That is all.

## How it works

- One Yjs document per vault, with two maps: `texts` (path → `Y.Text`) and `blobs` (path → `{hash, size, mtime}`).
- The plugin pushes every keystroke (debounced, 150 ms by default) into the shared document and applies remote changes directly into the open editor without moving your cursor.
- Files not open in an editor are written to disk; Obsidian's own file events feed changes back the other way.
- On startup the plugin does a three-way reconcile (disk, shared document, last-synced hash). Genuine conflicts, for example a file edited on both sides while offline for a long time and not mergeable, keep your copy as `Note (conflict Device 2026-10-14 2130).md`.
- The server speaks the standard [y-sync](https://github.com/yjs/y-protocols) protocol over WebSocket, persists every update to SQLite, and stores binary files content-addressed on disk.

## Server administration

```sh
replica-server user add alice          # create a user, prints their token
replica-server user token alice --label phone
replica-server user list
replica-server vault list
replica-server snapshot                # full-state snapshot of every vault + compaction
replica-server backup --out backup.tar.gz
replica-server backup --out - | age -r age1... > backup.tar.gz.age
replica-server restore --from backup.tar.gz --data-dir ./restored
```

## Layout

```
plugin/   Obsidian plugin (esbuild → main.js)
server/   Rust server (axum + yrs + SQLite)
docs/     DEVELOPMENT.md, STORAGE.md
```

## License

MIT
