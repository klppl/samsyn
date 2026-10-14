# Server storage model

Goals: many users, each with many vaults; every vault self-contained on disk; trivial to back up, snapshot, encrypt and restore after a disaster; room to add shared folders between users later.

## Layout

```
<data-dir>/
  server.json                       instance id, schema version
  registry.db                       SQLite: users, token hashes, vault index, (future) mounts
  users/
    <user-id>/
      vaults/
        <vault-id>/
          vault.json                owner, name, created (plain text, for recovery by hand)
          doc.db                    SQLite: append-only log of Yjs updates
          blobs/
            ab/abcdef…              immutable, content-addressed (SHA-256) binary files
          snapshots/
            2026-10-14T18-16-11Z.ybin   full Yjs state at that moment
            2026-10-14T18-16-11Z.json   manifest: counts, referenced blob hashes
```

IDs are random 16-hex strings so renaming a user or vault never moves data. `vault.json` carries the human-readable names so a directory is still meaningful without `registry.db`.

## Identity and access

- A **user** owns vaults. A user has one or more **tokens** (one per device is the intention). Only the SHA-256 of a token is stored; the plaintext is printed once by `user add` / `user token`.
- Clients present the token as `Authorization: Bearer` or `?token=` (mobile WebSocket cannot set headers).
- The vault name in the URL (`/ws/<name>`, `/blobs/<name>/<hash>`) is resolved **inside the authenticated user's namespace**. Two users with a vault named `notes` get two unrelated vaults. A vault is created on first use, so a new device needs nothing more than the token and the name.
- On a fresh data dir the server creates user `owner` and prints its token, so a single-person install needs no admin step.

## The document

One Yjs document per vault, two top-level maps:

| Map     | Key  | Value                          | Merge semantics                 |
|---------|------|--------------------------------|---------------------------------|
| `texts` | path | `Y.Text`                       | character-level CRDT merge      |
| `blobs` | path | `{hash, size, mtime}`          | last writer wins per key        |

Delete = remove the key. Rename = new key with a copy + remove old key. The server never interprets the content; it only stores and relays updates, so the schema can evolve on the plugin side.

## Durability

- Every update a client sends is appended to `doc.db` **before** it is broadcast (the persistence callback runs inside the commit). SQLite runs in WAL mode with `synchronous=NORMAL`: durable across process crashes, and across power loss except for the last few milliseconds.
- When a log exceeds 500 rows it is **compacted**: replaced by a single full-state update. This happens on load, after client disconnects, and on every snapshot. Compaction loses nothing (Yjs state is a complete merge of history).
- Blobs are written to a temp file and renamed into place, so a reader never sees a half-written file. They are immutable and content-addressed, so re-uploading is a no-op and the same bytes under two paths are stored once per vault.

## Snapshots

A snapshot is the entire document as one `.ybin` file plus a JSON manifest listing which blob hashes it references. They are:

- taken automatically for every loaded vault every `--snapshot-hours` (default 24) and on graceful shutdown, keeping the newest `--snapshot-keep` (default 14);
- taken on demand with `replica-server snapshot [user/vault]`;
- plain files, so any file-level backup tool (restic, borg, rsync, Time Machine) picks them up without caring about SQLite.

Roll a vault back with:

```sh
replica-server restore-snapshot owner/notes path/to/2026-10-14T18-16-11Z.ybin --yes
```

A safety snapshot of the current state is written first. Because Yjs merges, clients that still hold newer state will push it back after a rollback. That includes *deletions*: a device whose cached document contains "file X was deleted" will delete it again on reconnect. So a rollback is a three-step dance:

1. Stop the server, `restore-snapshot … --yes`, start the server.
2. On **every** device: Settings → Replica → **Reset local sync state** (drops the cached document; files on disk are untouched).
3. Each device reconnects, downloads the restored state, and reconciles its disk against it. Files that exist on disk but not in the snapshot are pushed up again as new files.

## Backups

`replica-server backup` writes a `tar.gz` that mirrors the data dir layout exactly. SQLite files go through the online backup API, so it is **safe to run while the server is serving**. Scope it with `--user` and `--vault`.

Encrypt by streaming to stdout:

```sh
replica-server backup --out - | age -r age1yourkey > replica-$(date +%F).tar.gz.age
# or
replica-server backup --out - | gpg -e -r you@example.com > replica.tar.gz.gpg
```

Restore into an empty dir, or merge selected vaults into a live one:

```sh
replica-server restore --from replica.tar.gz --data-dir /srv/replica/data
replica-server restore --from one-vault.tar.gz --merge      # adds/overwrites vaults in the archive
```

Because the archive is plain `tar`, a human can also `tar xzf` it and copy a single vault directory by hand in an emergency; `vault.json` says whose it is.

### Encryption at rest

The server does not encrypt data itself. Recommended: run on an encrypted filesystem or volume (LUKS, FileVault, cloud provider disk encryption) and encrypt backups as above. All disk writes go through `VaultStore`, so a per-vault at-rest cipher can be added later behind one interface without changing the layout.

## Disaster playbook

| Situation | Action |
|-----------|--------|
| Server disk lost | `restore --from <latest backup>`; devices reconnect and re-merge anything newer they still hold |
| One vault corrupted | `restore-snapshot user/vault <snapshot> --yes`, or `restore --merge` from a backup containing just that vault |
| `registry.db` lost, vault dirs intact | re-create users (`user add`), then `restore --merge` from any backup, or copy vault dirs and re-register them from their `vault.json` (tooling for this is a TODO) |
| Files vanished from the shared vault (a device pushed mass deletions) | Restore them from that device's Obsidian/system trash and let reconcile push them back, or roll back with `restore-snapshot` + reset local state on every device (see Snapshots). The plugin's mass-delete guard now refuses such pushes without explicit confirmation. |
| Token leaked | `user revoke <name> --label <label>` (or all), then `user token <name>` |
| Need to move servers | stop, `backup`, `restore` on the new host, point the plugin at the new URL |

## Shares and mounts

Shares are separate documents stored like vaults under `users/<uid>/shares/<share_id>/`. Registry tables `shares`, `share_grants` (hashed ticket secrets) and `mounts` describe who shares what and who mounted it. The server keeps shared subtrees equal between documents at runtime; see `docs/SHARING.md`. Backup and restore include share directories automatically because they sit inside the user's tree.

## Future: shared folders between users (server-to-server)

Goal from the plugin's point of view: "add my friend's `Projects/Shared` folder to my vault at `Shared/Anna`", after which it is just another folder that syncs. The plugin should not know whether the other user is on the same server or a different one.

Planned model, already reserved in the schema:

- A **share** is its own Yjs document (same `texts`/`blobs` shape) that holds one subtree. Its owner's vault *mounts* it at a path; the sharer's server is its home.
- A **mount** row (`mounts` table: `vault_id, mount_path, share_id, remote_server`) tells the server that paths under `mount_path` in that vault are backed by the share document instead of the vault document.
- The server composes what the client sees: updates for the mounted subtree are routed to/from the share document; the vault document never contains those keys. The client still speaks plain y-sync to one WebSocket.
- For a share whose home is another server, the local server keeps a replica of the share document and syncs it server-to-server over the same y-sync protocol (the server acts as a client to the remote server, authenticated with a share token issued by the remote owner). Blobs are fetched lazily from the home server and cached in the local vault's blob store.
- Permissions live with the share (read-only or read-write per grantee). Revocation = remote server stops accepting the share token; the local copy stays readable until the mount is removed.

Storage consequences are small: a share is a directory with the same `doc.db`/`blobs/`/`snapshots/` shape under `users/<owner>/shares/<share-id>/`, so backup, snapshot and restore work unchanged.
