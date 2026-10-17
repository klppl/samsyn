# Running the server

The server is a single Rust binary, `samsyn`. It stores each vault as a CRDT
document, relays changes between devices over WebSocket, and can also run as a
headless sync client. For hardening a public server (limits, what is exposed,
firewall) see [PUBLIC-SERVER.md](PUBLIC-SERVER.md); for the on-disk layout,
backups and snapshots see [STORAGE.md](STORAGE.md).

## Start it

```sh
cd server
cargo run --release -- serve
```

On first start it creates a user named `owner` and prints a token once:

```
INFO first start: created user 'owner'
INFO   token: rp_...
```

Data lives in `./data` by default (`--data-dir` or `SAMSYN_DATA_DIR` to change).

With Docker:

```sh
docker compose up -d
docker compose logs server | grep token
```

Prebuilt images are published by GitHub Actions
(`.github/workflows/docker.yml`) to the GitHub Container Registry on every push
to `main` and on tags:

```sh
docker pull ghcr.io/mrboatsman/samsyn:latest
```

`samsyn serve --help` lists every flag. The ones worth knowing: `--public-url`
(guest tickets), `--max-user-mb` / `--max-share-mb` / `--max-doc-mb` (quotas),
`--initial-token-file` (keep the first token out of logs), `--trust-proxy`
(behind a reverse proxy), `--p2p-port` / `--relay` / `--no-p2p`
(server-to-server), `--tls-cert` / `--tls-key`, `--max-blob-mb`,
`--snapshot-hours`. Each flag is also a `SAMSYN_*` environment variable.

## HTTPS

Obsidian on iOS runs in a secure WebKit context and refuses plain `ws://`
WebSockets, so the server must be reachable over `https://` with a certificate
the phone trusts. Desktop Obsidian accepts plain `http://` on a LAN, so you can
develop without TLS and add it when you bring in a phone.

**Tailscale (personal use, zero config).** Install Tailscale on the server and
the phone, then:

```sh
tailscale serve --bg --https=443 http://127.0.0.1:8787
```

Server URL in the plugin: `https://<machine>.<tailnet>.ts.net`. Valid
certificate, no port forwarding, works away from home.

**Caddy (public hostname).** With a DNS name pointing at the server:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy fetches a Let's Encrypt certificate automatically. Server URL:
`https://sync.example.com`.

**Built-in TLS.** If you already have a certificate:

```sh
samsyn serve --tls-cert fullchain.pem --tls-key privkey.pem
```

Server URL: `https://host:8787`. A self-signed certificate works only if its CA
is installed and trusted on the phone (Settings → General → About → Certificate
Trust Settings).

## Administration

```sh
samsyn user add alice          # create a user, prints their token
samsyn user token alice --label phone
samsyn user list
samsyn vault list
samsyn snapshot                # full-state snapshot of every vault + compaction
samsyn backup --out backup.tar.gz
samsyn backup --out - | age -r age1... > backup.tar.gz.age
samsyn restore --from backup.tar.gz --data-dir ./restored
```

## Headless client (no Obsidian)

The same binary can keep a plain directory in sync with a vault, for servers,
scripts, CI, or anything that reads and writes Markdown without Obsidian:

```sh
samsyn sync --server https://sync.example.com --token rp_... --vault notes --dir /srv/notes
```

- Watches the directory; a file save is pushed within ~300 ms and merged like an
  editor edit. Concurrent edits to different parts of the same file merge;
  overlapping edits keep a `(conflict …)` copy.
- Remote changes are written to disk; removed files go to `<dir>/.samsyn/trash/`
  instead of being deleted.
- Works offline and catches up on reconnect. Bookkeeping lives in
  `<dir>/.samsyn/` (do not copy that folder to another machine).
- `--once` connects, syncs, and exits when idle. Good for cron or a deploy step.
- Same mass-delete guard as the plugin: removing most of the vault from the
  directory is held back unless `--allow-mass-delete` is set.
- Folders mounted read-only from someone else's share are respected: edits there
  are not sent, the server's version is put back, new files move to
  `<mount> (read-only copy)/`, and the files are marked read-only on disk.
- Shared folders (your shares, mounts) are protected like in the plugin: moving
  them on disk, or moving files out of or into them, is undone with a warning in
  the log; use the plugin's Move… to relocate a mount.
- Guest mode, no account needed: `samsyn sync --ticket <ticket> --dir /srv/x`
  syncs one shared folder straight from the owner's server (the ticket must come
  from a server started with `--public-url`). A revoked ticket makes the client
  exit.

All flags can be environment variables (`SAMSYN_SERVER`, `SAMSYN_TOKEN`,
`SAMSYN_VAULT`, `SAMSYN_DIR`). A systemd unit:

```ini
[Unit]
Description=Samsyn sync for /srv/notes
After=network-online.target

[Service]
Environment=SAMSYN_SERVER=https://sync.example.com
Environment=SAMSYN_TOKEN=rp_...
Environment=SAMSYN_VAULT=notes
Environment=SAMSYN_DIR=/srv/notes
ExecStart=/usr/local/bin/samsyn sync
Restart=always

[Install]
WantedBy=multi-user.target
```
