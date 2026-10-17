# Samsyn

Conflict-free live sync for [Obsidian](https://obsidian.md), self-hosted.

> **Samsyn** [ˈsamːsyːn] is a Swedish word meaning "shared view" or "common
> understanding", the state where two people see things the same way. It's built
> from *sam* (together, shared) and *syn* (sight, view). The name fits twice
> over: your devices all see the same notes, and so does the friend you share a
> vault with. It also happens to sound a lot like *sync*.

Run one small server on a machine you own. Every device editing the same note
converges character by character, live, with no conflict files. Share a single
folder with a friend and both of you edit it in real time. Works on desktop, iOS
and Android, offline and back.

> Status: early development. Not yet published to the Obsidian community plugin list.

## How it compares

| | Conflicts | Real-time | Self-hosted | Mobile | Share a folder | Free |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| **iCloud / Dropbox** | Conflicted copies | No | No | Yes | No | Yes |
| **Obsidian Sync** | Rare | Delayed | No (hosted) | Yes | No | $96/yr |
| **[Remotely Save](https://github.com/remotely-save/remotely-save)** | Conflicted copies | No | S3 / WebDAV / … | Yes | No | Yes |
| **[Obsidian Git](https://github.com/Vinzent03/obsidian-git)** | Manual merge | No | Any git host | Limited | No | Yes |
| **[Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync)** | Prompts you | Yes | CouchDB | Yes | No | Yes |
| **Syncthing** | Conflicted copies | Near | Yes | Android only | No | Yes |
| **[YAOS](https://community.obsidian.md/plugins/yaos)** | CRDT merge | Yes | Cloudflare Worker | Yes | No | $0 |
| **[Ilow Sync](https://community.obsidian.md/plugins/ilow-sync)** | CRDT merge | Yes | Go + PostgreSQL | Yes | No | Yes |
| **Samsyn** | CRDT merge | Yes | One binary | iOS + Android | Any subfolder | $0 |

Text merges with [Yjs](https://github.com/yjs/yjs) in the plugin and
[yrs](https://github.com/y-crdt/y-crdt) on the server, so simultaneous edits to
one note just merge. Binary files (images, PDFs) sync whole, last writer wins.

Samsyn shares the CRDT, real-time, self-hosted approach of YAOS and Ilow Sync.
What sets it apart: you share **any subfolder** with another person and both edit
it live (even across two separate servers, over an encrypted peer-to-peer link),
and the whole server is **one binary** with no database, cloud account or runtime
to deploy. iOS and Android are both first-class.

## Quick start

**1. Run the server.**

```sh
cd server
cargo run --release -- serve
```

It prints a one-time token on first start:

```
INFO first start: created user 'owner'
INFO   token: rp_...
```

That is all you need to try it on a LAN. For HTTPS (required for iOS), Docker, a
prebuilt image, admin commands and the headless client, see
[docs/SERVER.md](docs/SERVER.md). To run it on the public internet, see
[docs/PUBLIC-SERVER.md](docs/PUBLIC-SERVER.md).

**2. Install the plugin.** Until it is in the community list, install it manually
or with [BRAT](https://github.com/TfTHacker/obsidian42-brat). See
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for desktop and phone instructions.

**3. Connect.** Obsidian → Settings → Samsyn:

| Setting    | Value                                                        |
|------------|--------------------------------------------------------------|
| Server URL | `http://your-host:8787` (or `https://…` behind a reverse proxy) |
| Token      | the `rp_…` token printed by the server                       |
| Vault ID   | defaults to the vault name; use the same ID on every device  |

Press **Test connection**, then **Reconnect**. Repeat on each device with the
same server and vault ID. Easiest for a second device: Settings → Server →
**Devices** → *Set up another device…* shows a QR code; scan it and Obsidian
connects.

## Sharing a folder with someone else

Share part of a vault, not all of it, with live editing on both sides.

1. Owner: right-click a folder or note → **Samsyn: Share…**, pick read & write or
   read only, copy the ticket. One ticket per person so you can revoke them
   separately.
2. Friend: Settings → Samsyn → **Sharing** → *Add from ticket…*, paste, choose
   where it goes, **Mount**. The files appear and stay live both ways. A QR code
   and `obsidian://samsyn` link work too.

The friend does not even need their own server: start yours with `--public-url`
and the ticket lets them join directly (guest mode). Two servers sharing a folder
connect over an encrypted [iroh](https://iroh.computer) link with no open ports.
Full design in [docs/SHARING.md](docs/SHARING.md) and
[docs/GUEST-MODE.md](docs/GUEST-MODE.md).

You can also sync chosen community plugins and their settings between your own
devices (Settings → Samsyn → **Plugins**), e.g. Iconize's icons and colours.

## Documentation

| Doc | What |
|---|---|
| [SERVER.md](docs/SERVER.md) | Running the server, HTTPS, admin CLI, headless client |
| [PUBLIC-SERVER.md](docs/PUBLIC-SERVER.md) | Exposing a server safely: limits, firewall, hardening |
| [SHARING.md](docs/SHARING.md) | How folder sharing and server-to-server links work |
| [GUEST-MODE.md](docs/GUEST-MODE.md) | Joining a share with only a ticket, no account |
| [STORAGE.md](docs/STORAGE.md) | On-disk layout, backups, snapshots |
| [DEVELOPMENT.md](docs/DEVELOPMENT.md) | Building the plugin and server, local cache, internals |
| [SECURITY.md](docs/SECURITY.md) | Threat model and security process |

## Layout

```
plugin/   Obsidian plugin (TypeScript, esbuild → main.js)
server/   Rust server + headless client (axum + yrs + SQLite)
docs/     documentation
```

## License

MIT
