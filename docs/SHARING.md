# Sharing part of a vault with another user

Goal: share one folder or one note from my vault with another person, with live sync, without sharing the whole vault and without exposing either server to the internet. From the user's point of view: pick a folder → copy a ticket → the other person pastes it → both see the same folder and edit it live.

## Principles

1. **The plugin does not know about sharing.** It syncs one vault document with its server exactly as today. Its only sharing-related features are UI calls to the server: create a share, show the ticket, mount a ticket, revoke, unshare.
2. **The server composes.** It knows which paths are shared and keeps the shared subtree equal between the owner's vault document and a separate *share document*; on the other side it keeps the share document equal to the subtree at the mount path in the friend's vault document.
3. **One ticket = one share, fixed at creation.** A ticket grants access to exactly the folder or note it was created for. Changing what a ticket covers is not supported; create a new share instead.
4. **Server-to-server over iroh.** The share document is replicated between the two servers over an encrypted QUIC connection with NAT traversal; neither server needs an open port.

## Why a bridge instead of a shared document

Mirroring edits between two CRDT documents by index is unsafe in general: if both documents advance concurrently, replayed indices point at the wrong characters. The server can make it safe because it is the single point where all updates to both documents are applied:

- Every update to any bridged document is applied under one server-wide lock.
- Applying an update yields the exact *delta* that document experienced (yrs text events). Immediately after the apply, still under the lock, the delta is replayed into the bridged document(s) as a plain edit.
- Therefore after every step the bridged texts are byte-for-byte equal, and the next incoming update, whichever document it targets, is merged by that document's CRDT first and only its resulting delta is mirrored.

Concurrency between clients is resolved by the CRDT of the document the client talks to; the bridge only ever replays already-merged results. The cost is serialization of CRDT applies on the server, which is microseconds per update and irrelevant at the scale of a personal or small-team server.

Crash between apply and mirror leaves the two texts unequal. On startup the bridge reconciles every shared key with a text diff, treating the document nearest the source of truth as authoritative (owner's vault on the owner's server, share document on the friend's server). At worst the last few milliseconds of one side's edits move into the other's text as a plain edit; nothing is lost.

## Model

```
owner's vault doc  ──bridge──  share doc  ══iroh══  share doc replica  ──bridge──  friend's vault doc
   Projects/X/…                   …                     …                          Shared/X/…
```

- **Share**: `{ id, owner, vault, name, prefix, only_file? }`. `prefix` is the folder (`Projects/X/`); `only_file` narrows it to one note (`prefix = Notes/`, `only_file = a.md`). Share keys are relative to `prefix`.
- **Grant** (= ticket): `{ share, secret_hash, label, access: rw|ro, revoked_at }`. Many grants per share; revoke one without affecting others.
- **Mount**: `{ id, user, vault, share, mount_prefix, access, home_node, home_addrs, only_file, secret }`. A mount bridges the share into the friend's vault at `mount_prefix`. `home_node` is the owner's server; when it equals this server the share is local (two users on one server) and no network is involved. Otherwise the server keeps a local *replica* of the share document and links it to the owner's server (below).
- **Ticket**: `replica` + base32 of `{ v, share_id, name, node_id, secret, access, addrs }`. `addrs` are the owner server's relay URL and direct addresses at the time the ticket was made (`relay:https://…`, `ip:host:port`); the node id alone is enough when a relay is in use.

Overlapping shares or mounts (nested prefixes) in one vault are refused. A mount prefix must be empty in the vault when mounted, so nothing is overwritten.

## Storage

Share documents use the same store type as vaults, so snapshot, backup and restore are unchanged:

```
users/<uid>/shares/<share_id>/    vault.json, doc.db, blobs/, snapshots/      (owned shares)
users/<uid>/mounts/<mount_id>/    vault.json, doc.db, blobs/, snapshots/      (replicas of shares on other servers)
```

Registry tables: `shares`, `share_grants`, `mounts`. The server's iroh identity is `data/identity.key`; its public key is the `node_id` in tickets.

Blobs: when a blob entry crosses a bridge the bytes are copied into the target store so each store stays self-contained and the friend's clients fetch from their own server. Across a server link the bytes are fetched on demand (below).

## Server to server (phase 2)

Every server binds an [iroh](https://iroh.computer) endpoint with its identity key (UDP, `--p2p-port`, default 8788) and accepts connections on ALPN `replica/share/1`. Connections are QUIC, end-to-end encrypted and authenticated by the node ids; iroh dials through a relay first and upgrades to a direct path when NAT traversal succeeds. Neither server needs an open port or a public address. The default relays are the public n0 ones; `--relay <url>` selects your own ([self-hosting a relay](https://github.com/n0-computer/iroh/tree/main/iroh-relay)), `--relay none` disables relays (LAN / VPN only), `--no-p2p` turns the endpoint off.

When a ticket from another server is mounted:

1. The mounting server connects and does a hello handshake (share id + ticket secret) before storing anything, so a bad or revoked ticket fails immediately with a clear error. The owner answers with the granted access level, share name and single-note flag.
2. It creates a replica document under `users/<uid>/mounts/<mount_id>/`, bridges it into the vault at the mount path exactly like a local share (truth = replica), and starts a *link* task.
3. The link keeps one connection open with one long-lived control stream carrying y-sync messages both ways, framed as `u32 length + payload`. On the owner side this stream is treated like any WebSocket client of the share document; for a read-only grant the owner drops incoming document updates. Reconnects use exponential backoff (2 s … 60 s); the owner re-checks the grant every 30 s and closes the link when it is revoked or the share is deleted, after which the mounting server marks the mount revoked and removes the bridge (files stay).
4. Blobs: whenever either side's copy of the share document references a hash its store lacks, it opens a short blob stream (`frame(hash)` → `frame(bytes)`), verifies the SHA-256, stores the bytes and copies them to the bridged vault's store. The vault's clients may see the metadata a moment before the bytes exist; both the plugin and the headless client retry a missing blob with backoff.

Ticket secrets never leave the two servers; users still only talk to their own server with their own token. Mount status (`connected`, `reconnecting (…)`, `revoked: …`) is shown in the Sharing settings.

Limits on the link (the node id is public, so everything before the hello must be cheap): a hello or hash frame is capped at 4 KB / 256 B before allocation, a blob frame at the server's `--max-blob-mb`, sync frames at 256 MB; at most 16 streams per connection are served at once. A hello with an unknown protocol version gets `VersionMismatch` and the mounting side keeps retrying with backoff (status says so); only a real grant denial revokes the mount.

## Semantics

- Owner creates share → bridge created, share document filled from the vault subtree.
- Friend mounts → share subtree copied into their vault at the mount prefix; their plugin sees new files arrive like any other remote change.
- Edits on either side: client → its vault doc → bridge → share doc → bridge → other vault doc → other client. All live.
- Rename across the boundary: delete on one side, create on the other (as renames already work).
- Read-only mount: the friend's edits under the mount prefix are reverted by the server to the share's content, so the friend's document stays equal to the share. A file the friend creates under the mount (one the share does not have) is not thrown away: before the revert the server copies it to a sibling folder outside the mount, `<mount> (read-only copy)/<same relative path>`, and logs a warning. The same happens if the owner deletes a file while the friend has local content for it.
- Blobs: when a bridge replays a binary file it copies the bytes into the target store *before* inserting the metadata, so a client that fetches the moment the metadata arrives finds the bytes. The plugin additionally retries a missing blob with backoff (2 s, 5 s, 15 s, 30 s) in case an uploader is slow.
- Share and mount creation run under one server-wide lock (overlap check, registry insert, bridge attach), so two concurrent requests cannot both pass the overlap check.
- Moving things around: inside a shared folder you can reorganise freely. Moving the shared folder itself (a mount, your own shared folder, or an ancestor of either) in the file explorer is **blocked**: the plugin moves it straight back and shows one notice pointing at Settings → Sharing → **Move…**, which does the move properly (`PATCH /api/mounts/{id}` / `/api/shares/{id}`: bridge detached, keys renamed inside the vault document, bridge re-attached, so the other side never sees a deletion; guest mounts just update their path). Moving a note or folder *out of*, *into* or *between* shared areas is blocked the same way, with a notice explaining that it would disappear for (or be shared with) other people; copy instead. Obsidian reports a rename for every file inside a moved folder, so the plugin handles the top-level move once and ignores the children and its own revert. The headless client does the same from what it can see on disk: a shared folder (or note) that vanishes and reappears elsewhere in the same batch of file events is moved back, a file whose content leaves or enters a shared area is moved back, and a shared folder that simply disappears has its deletions held back (restart with `--allow-mass-delete` to really remove it for everyone).
- Revoke grant (confirmed in the UI, since it cuts people off): mounts made with that ticket stop bridging, guests are disconnected. Files stay in the friend's vault as ordinary files. Revoked tickets are hidden behind a "N revoked tickets" toggle and can be **restored** (the same ticket works again; mounts on this server resume by themselves, guests reconnect, mounts on other servers have to be re-mounted) or **deleted** from the list.
- Unshare: bridge removed, share document deleted, owner's files untouched; all mounts of it go inert.
- Unmount: bridge removed, friend's files stay.

## Guests (no server of their own)

A person without a replica server can still join a share: the plugin (*Add from ticket…* → **Mount as guest**, or simply **Mount** when no server is configured) or the headless client (`replica-server sync --ticket <ticket> --dir …`) connects straight to the **owner's** server and syncs the share document, authenticated by the ticket secret. In the plugin each guest mount is its own engine with its own document, scoped to one folder; the home engine excludes those folders, so nothing from a guest folder ever reaches your own server and nothing else in your vault reaches the owner. Read-only guests never push; their local edits are overwritten by the shared version (kept as a conflict copy when they differ at reconcile time). The owner's server must be reachable over HTTPS and started with `--public-url https://…`, which is embedded in every ticket it issues (ticket v2; v1 tickets still decode). Full design in `GUEST-MODE.md`.

Guest routes, all authenticated by `(share id, secret)` given as `?secret=` or `Authorization: Bearer`:

| Method | Path | Notes |
|---|---|---|
| GET | `/guest/shares/{id}` | name, access, folder or note; validates the ticket |
| GET | `/guest/ws/{id}` | y-sync WebSocket on the share document; read-only grants have their document updates dropped server-side |
| GET/PUT | `/guest/blobs/{id}/{hash}` | share-scoped blobs; PUT only for read & write |

Hygiene on these public routes: unknown share and bad secret answer identically (403), more than 10 failures per IP in 10 minutes gives 429, and the request log carries the path only, never the query string. The owner re-checks the grant every 30 s on open guest sockets and closes them when the ticket is revoked or the share deleted; the headless client then exits with a clear message. Each guest connection stamps `last_seen_at` on its ticket, shown to the owner.

## API (user token auth, same as everything else)

| Method | Path | Body → Result |
|---|---|---|
| GET | `/api/vaults/{vault}/sharing` | → node_id, shares (with grants), mounts |
| POST | `/api/vaults/{vault}/shares` | `{path, name?, access?}` → share + first ticket |
| POST | `/api/shares/{id}/tickets` | `{label?, access?}` → ticket |
| DELETE | `/api/shares/{id}/grants/{label}` | revoke; on an already revoked ticket: delete it from the list |
| POST | `/api/shares/{id}/grants/{label}/restore` | un-revoke: same ticket valid again, local mounts re-attached (remote mounts must be mounted again) |
| DELETE | `/api/shares/{id}` | unshare |
| POST | `/api/vaults/{vault}/mounts` | `{ticket, mount_path}` → mount |
| PATCH | `/api/mounts/{id}` | `{path}` → move the mount to another folder (keys renamed server-side, nothing deleted) |
| PATCH | `/api/shares/{id}` | `{path}` → move the shared folder or note within the owner's vault |
| DELETE | `/api/mounts/{id}` | unmount |
| POST | `/api/tickets/inspect` | `{ticket}` → name, access, same_server, valid (local tickets), from_own_vault |

## Plugin UI

Settings → **Sharing**: *Shared by me* (create from folder/note path, list with ticket copy, new ticket, revoke, unshare) and *Shared with me* (paste ticket + mount path, list, unmount). File explorer context menu: **Replica: Share…**. Nothing else in the plugin changes.

## Phases

1. **One server** (done). Share docs, bridges, reconcile, API, tickets with local `node_id`, plugin Sharing UI. Two users on the same server collaborate on a folder.
2. **iroh** (done). Endpoint, accept loop, `replica/share/1` protocol (y-sync framed over a QUIC stream + blob fetch), remote mounts with local replicas, reconnect, relay config. Scenario: `server/scripts/remote-e2e.sh`.
3. **Polish** (done except the relay guide). Presence across the bridge (`server/src/presence.rs`: awareness states of clients inside a shared subtree are mirrored into the share and every mounting vault under a synthetic client id with the path remapped and a `_via` marker; scenario `server/scripts/presence-e2e.sh`), headless client refuses edits under read-only mounts and marks those files read-only on disk, sharing UI reworked (tabs, modals, ticket inspection via `POST /api/tickets/inspect`, revocation notices).
