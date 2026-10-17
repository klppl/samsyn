# Running a replica server that strangers can reach

You need this when you use **guest mode** (people join your shares with only a ticket) or when your own phone connects from outside your network. Server-to-server sharing over iroh does *not* need any of it.

## What is exposed

| Route | Who can call it | Authenticated by |
|---|---|---|
| `/ws/{vault}`, `/blobs/{vault}/…`, `/api/…` | your users | user token (`rp_…`), one per device |
| `/guest/session/{share}`, `/guest/ws/{share}`, `/guest/shares/{share}`, `/guest/blobs/{share}/…` | anyone holding a ticket | ticket secret, or a session id obtained with it |
| `/health`, `/` | anyone | nothing (no data beyond the node id) |

Everything else is 404. There is no admin interface over HTTP; users and tokens are created on the host (`replica-server user add`) or by a logged-in user for their own devices (`POST /api/me/tokens`).

## What a leaked ticket can and cannot do

A ticket is a bearer credential **for one share**. Whoever has it can read that folder or note, and write to it if the ticket is read & write. It cannot reach anything else in your vault, cannot list your shares or users, and cannot issue new tickets. Revoke it (Settings → Sharing → ticket → Revoke) and it is dead within 30 seconds for every connection that used it; issue a new one for the people who should stay.

Issue one ticket per person. Then a leak costs you one revoke, not a re-invite of everyone.

## What a leaked user token can do

Everything that user can do: read and write all their vaults, create shares, issue tickets. Treat it like a password. Use one token per device (Settings → Server → Devices → *Set up another device…*) so a lost phone means revoking one token. Tokens are stored hashed on the server; the plaintext exists only on the device.

## Built-in protection on `/guest/*`

- Unknown share and wrong secret answer identically (`403`), so nothing can be enumerated.
- More than 10 failed authentications from one IP address within 10 minutes → `429` for the rest of the window. Behind a reverse proxy the client IP is taken from `X-Forwarded-For`; make sure your proxy sets it, or every guest shares one bucket.
- The request log carries the path only, never the query string, so secrets and session ids do not end up in log files.
- Sessions: the plugin presents the secret once (`POST /guest/session/{share}`) and uses the returned session id from then on; sessions expire after 7 days and die immediately when the ticket is revoked.
- Upload quota per share for guests: `--max-share-mb` (default 2048, `0` = unlimited). The owner's own uploads are not limited by it.
- Blob size limit for everyone: `--max-blob-mb` (default 256). Vaults per user: `--max-vaults-per-user` (default 50); reading never creates a vault.
- Live sockets re-check their credential every 30 s: a revoked device token or ticket ends the connection within that time.
- Behind a reverse proxy, start with `--trust-proxy` so the guest rate limiter sees the real client address (last `X-Forwarded-For` entry). Never set it when clients can reach the server directly.

## Limits and their defaults

Every limit is a flag (or `REPLICA_*` env var); the defaults are chosen so a server that nobody tuned is still safe and stable, and generous enough that normal use never hits them.

| Flag | Default | Protects against |
|---|---|---|
| `--max-blob-mb` | 256 | one huge upload |
| `--max-share-mb` | 2048 | guests filling a share's blob store |
| `--max-user-mb` | 20480 | one user filling the disk (blobs + document logs; `/me` shows usage) |
| `--max-doc-mb` | 256 | a collaborator growing a document without end (text only) |
| `--max-vaults-per-user` | 50 | vault creation by name; reads never create |
| `--max-conns-per-token` | 16 | one token parking many sockets |
| `--max-conns-per-ticket` | 8 | one ticket parking many sockets |
| `--max-conns-per-peer` | 4 | one peer server parking many p2p connections (64 in total) |
| `--idle-unload-minutes` | 30 | memory growth from vaults nobody is using |
| (fixed) | 10 / 10 min | failed ticket auth per address; 30 / 10 min failed token auth; 10 / 10 min failed p2p hellos per node |
| (fixed) | 6 / min | full-state requests per socket |
| (fixed) | 64 | awareness client ids per socket; read-only guests send at most 4 MB per message |
| `--cors-origin`, `--cors-any` | Obsidian origins only | browsers calling the API from other sites (the plugin does not need CORS) |
| `--initial-token-file` | unset | the owner token ending up in log storage |
| `--trust-proxy` | off | believing `X-Forwarded-For` from anyone |

Secrets at rest: user tokens and ticket secrets are stored hashed; mount secrets (which must be presented again) are sealed with XChaCha20-Poly1305 under a key derived from `identity.key`. Keep `identity.key` with the data dir: without it the mounts on this server cannot reconnect.

## Recommended setup

1. **TLS is mandatory.** Phones refuse plain `ws://`, and tickets must not travel in clear text. Either a reverse proxy with automatic certificates, or `--tls-cert/--tls-key`.
2. **Reverse proxy with Caddy** (simplest; certificates from Let's Encrypt automatically):

   ```caddyfile
   sync.example.com {
       reverse_proxy 127.0.0.1:8787
   }
   ```

   Caddy forwards WebSockets and sets `X-Forwarded-For` by default. Start the server bound to localhost and with the public URL that goes into tickets:

   ```sh
   replica-server serve --bind 127.0.0.1:8787 --public-url https://sync.example.com
   ```

3. **Rate limiting at the proxy** on top of the built-in one, if the host is a shared box. Caddy: the `rate_limit` plugin; nginx: `limit_req_zone`. Limit `/guest/*` harder than the rest.
4. **Keep the p2p port closed** unless you use server-to-server shares (then forward UDP `--p2p-port`, or rely on relays and forward nothing).
5. **Back up** `data/` (see STORAGE.md). `registry.db` holds token hashes and ticket secrets of mounts; treat backups as sensitive.
6. **Watch the log** for `guest auth failed` bursts and `quota reached`; both are visible at `info` level.

## Firewall summary

| Port | Protocol | Needed for |
|---|---|---|
| 443 | TCP | everything (through the proxy) |
| 8787 | TCP | only if you skip the proxy and use `--tls-cert` directly |
| 8788 | UDP | optional, server-to-server shares without a relay |
