# Sharing part of a vault with another user

Goal: share one folder or one note from my vault with another person, with live sync, without sharing the whole vault and without exposing either server to the internet. From the user's point of view: pick a folder → copy a ticket → the other person pastes it → both see the same folder and edit it live.

## Principles

1. **The plugin does not know about sharing.** It syncs one vault document with its server exactly as today. Its only sharing-related features are UI calls to the server: create a share, show the ticket, mount a ticket, revoke, unshare.
2. **The server composes.** It knows which paths are shared and keeps the shared subtree equal between the owner's vault document and a separate *share document*; on the other side it keeps the share document equal to the subtree at the mount path in the friend's vault document.
3. **One ticket = one share, fixed at creation.** A ticket grants access to exactly the folder or note it was created for. Changing what a ticket covers is not supported; create a new share instead.
4. **Server-to-server over iroh** (phase 2). The share document is replicated between the two servers over an encrypted QUIC connection with NAT traversal; neither server needs an open port.

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
- **Mount**: `{ id, user, vault, share, mount_prefix, access, home_node, secret }`. A mount bridges the share into the friend's vault at `mount_prefix`. `home_node` is the owner's server; when it equals this server the share is local (two users on one server) and no network is involved.
- **Ticket**: `replica` + base32 of `{ v, share_id, name, node_id, secret, access, relay/addrs }`.

Overlapping shares or mounts (nested prefixes) in one vault are refused. A mount prefix must be empty in the vault when mounted, so nothing is overwritten.

## Storage

Share documents use the same store type as vaults, so snapshot, backup and restore are unchanged:

```
users/<uid>/shares/<share_id>/    share.json, doc.db, blobs/, snapshots/      (owned shares)
users/<uid>/mounts/<share_id>/    mount.json, doc.db, blobs/, snapshots/      (replicas of remote shares, phase 2)
```

Registry tables: `shares`, `share_grants`, `mounts`. The server's iroh identity is `data/identity.key`; its public key is the `node_id` in tickets.

Blobs: when a blob entry crosses a bridge the bytes are copied (hard-linked when possible) into the target store so each store stays self-contained and the friend's clients fetch from their own server.

## Semantics

- Owner creates share → bridge created, share document filled from the vault subtree.
- Friend mounts → share subtree copied into their vault at the mount prefix; their plugin sees new files arrive like any other remote change.
- Edits on either side: client → its vault doc → bridge → share doc → bridge → other vault doc → other client. All live.
- Rename across the boundary: delete on one side, create on the other (as renames already work).
- Read-only mount: the friend's edits under the mount prefix are reverted by the server to the share's content, so the friend's document stays equal to the share.
- Revoke grant: mounts made with that ticket stop bridging. Files stay in the friend's vault as ordinary files.
- Unshare: bridge removed, share document deleted, owner's files untouched; all mounts of it go inert.
- Unmount: bridge removed, friend's files stay.

## API (user token auth, same as everything else)

| Method | Path | Body → Result |
|---|---|---|
| GET | `/api/vaults/{vault}/sharing` | → node_id, shares (with grants), mounts |
| POST | `/api/vaults/{vault}/shares` | `{path, name?, access?}` → share + first ticket |
| POST | `/api/shares/{id}/tickets` | `{label?, access?}` → ticket |
| DELETE | `/api/shares/{id}/grants/{label}` | revoke |
| DELETE | `/api/shares/{id}` | unshare |
| POST | `/api/vaults/{vault}/mounts` | `{ticket, mount_path}` → mount |
| DELETE | `/api/mounts/{id}` | unmount |

## Plugin UI

Settings → **Sharing**: *Shared by me* (create from folder/note path, list with ticket copy, new ticket, revoke, unshare) and *Shared with me* (paste ticket + mount path, list, unmount). File explorer context menu: **Replica: Share…**. Nothing else in the plugin changes.

## Phases

1. **One server.** Share docs, bridges, reconcile, API, tickets with local `node_id`, plugin Sharing UI. Two users on the same server collaborate on a folder.
2. **iroh.** Identity already exists; add endpoint, accept loop, `replica/share/1` protocol (y-sync framed over a QUIC stream + blob fetch), remote mounts, reconnect, relay config.
3. **Polish.** Presence across the bridge, headless client awareness of read-only, revocation UX, docs for self-hosted relay.
