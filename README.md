# Replica

Conflict-free live sync for [Obsidian](https://obsidian.md), self-hosted.

Replica is two parts:

- **`plugin/`** — an Obsidian plugin (TypeScript, no native code, runs on desktop, iOS and Android).
- **`server/`** — a small Rust server that stores each vault as a CRDT document and relays changes between devices in real time. The same binary also runs as a headless client (`replica-server sync`) to keep a plain directory in sync without Obsidian.

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

Prebuilt images are published by CI (`.github/workflows`) to the
the private tracker registry on every push to `main` and on tags:

```sh
docker pull ghcr.io/mrboatsman/samsyn:latest
```

### 1b. HTTPS (required for iOS, recommended everywhere)

Obsidian on iOS runs in a secure WebKit context and refuses plain `ws://` WebSockets, so the server must be reachable over `https://` with a certificate the phone trusts. Three easy ways:

**Tailscale (personal use, zero config).** Install Tailscale on the server and the phone, then:

```sh
tailscale serve --bg --https=443 http://127.0.0.1:8787
```

Server URL in the plugin: `https://<machine>.<tailnet>.ts.net`. Valid certificate, no port forwarding, works away from home.

**Caddy (public hostname).** With a DNS name pointing at the server:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy fetches a Let's Encrypt certificate automatically. Server URL: `https://sync.example.com`.

**Built-in TLS.** If you already have a certificate (Let's Encrypt via certbot, or your own CA installed on the phone):

```sh
replica-server serve --tls-cert fullchain.pem --tls-key privkey.pem
```

Server URL: `https://host:8787`. A self-signed certificate works only if its CA is installed and trusted on the phone (Settings → General → About → Certificate Trust Settings).

Desktop Obsidian accepts plain `http://` on a LAN, so you can develop without TLS and add it when you bring in a phone.

### 2. Install the plugin

Until it is in the community list, install it manually or with [BRAT](https://github.com/TfTHacker/obsidian42-brat). See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for desktop and phone instructions.

### 3. Connect

Obsidian → Settings → Replica:

| Setting    | Value                                                        |
|------------|--------------------------------------------------------------|
| Server URL | `http://your-host:8787` (or `https://…` behind a reverse proxy) |
| Token      | the `rp_…` token printed by the server                       |
| Vault ID   | defaults to the vault name; use the same ID on every device  |

Press **Test connection** (checks server and token), then **Reconnect**. The **Status** section at the top of the settings page shows live state, last error and hints; on desktop the status bar shows it too. The command **Replica: Show sync status** works on every platform.

Repeat on each device with the same server, token and vault ID. That is all.

## How it works

The plugin caches the document and its sync bookkeeping in IndexedDB on the device (not in the vault folder), so typing costs one small write and a copied `.obsidian` never carries another device's state. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md#local-cache-on-the-device).

- One Yjs document per vault, with two maps: `texts` (path → `Y.Text`) and `blobs` (path → `{hash, size, mtime}`).
- The plugin pushes every keystroke (debounced, 150 ms by default) into the shared document and applies remote changes directly into the open editor without moving your cursor.
- Files not open in an editor are written to disk; Obsidian's own file events feed changes back the other way.
- On startup the plugin does a three-way reconcile (disk, shared document, last-synced hash). Genuine conflicts, for example a file edited on both sides while offline for a long time and not mergeable, keep your copy as `Note (conflict Device 2026-10-14 2130).md`.
- The server speaks the standard [y-sync](https://github.com/yjs/y-protocols) protocol over WebSocket, persists every update to SQLite, and stores binary files content-addressed on disk.

## Sharing a folder or note with someone else

Share part of a vault, not all of it, with live editing on both sides. The plugin stays a plain one-vault sync client; the server does the composition (see [docs/SHARING.md](docs/SHARING.md)).

1. Owner: right-click a folder or note → **Replica: Share…** (or Settings → Replica → **Sharing** → *Share a folder or note…*). Pick read & write or read only, **Create share**, copy the ticket. One ticket = exactly that folder or note; issue one ticket per person so you can revoke them separately.
2. Friend: Settings → Replica → **Sharing** → *Add from ticket…*: paste, see what it is (name, access, which server), choose where it goes (default `Shared/X`), **Mount**. The files appear and stay live in both directions.
3. While both edit, each sees the other's presence inside the shared folder. Revoke a ticket, or unshare, at any time; the friend is told in their Sharing tab and keeps their copy as ordinary notes.

The two users can be on the same server or on different ones. Between servers the share travels over an encrypted [iroh](https://iroh.computer) connection: no open ports, no public address, NAT traversal through a relay (public n0 relays by default, `--relay <url>` for your own, `--relay none` for LAN only). The ticket carries the owner server's node id and addresses; the friend's server verifies the ticket with the owner before mounting, then keeps a local replica in sync. `--p2p-port` (UDP, default 8788) is worth keeping fixed; forwarding it is optional. `--no-p2p` turns all of this off.

## Headless client (no Obsidian)

The same binary can keep a plain directory in sync with a vault, for servers, scripts, CI, or anything that reads and writes Markdown without Obsidian:

```sh
replica-server sync --server https://sync.example.com --token rp_... --vault notes --dir /srv/notes
```

- Watches the directory; a file save is pushed within ~300 ms and merged like an editor edit. Concurrent edits to different parts of the same file merge; overlapping edits keep a `(conflict …)` copy.
- Remote changes are written to disk; removed files go to `<dir>/.replica/trash/` instead of being deleted.
- Works offline and catches up on reconnect. Bookkeeping lives in `<dir>/.replica/` (do not copy that folder to another machine).
- `--once` connects, syncs, and exits when idle. Good for cron or a deploy step.
- Same mass-delete guard as the plugin: removing most of the vault from the directory is held back unless `--allow-mass-delete` is set.

All flags can be environment variables (`REPLICA_SERVER`, `REPLICA_TOKEN`, `REPLICA_VAULT`, `REPLICA_DIR`). A systemd unit:

```ini
[Unit]
Description=Replica sync for /srv/notes
After=network-online.target

[Service]
Environment=REPLICA_SERVER=https://sync.example.com
Environment=REPLICA_TOKEN=rp_...
Environment=REPLICA_VAULT=notes
Environment=REPLICA_DIR=/srv/notes
ExecStart=/usr/local/bin/replica-server sync
Restart=always

[Install]
WantedBy=multi-user.target
```

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
