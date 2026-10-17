//! Server-to-server share sync over iroh (QUIC with relay-assisted NAT traversal).
//!
//! The owner server accepts connections on ALPN `replica/share/1`. A mounting server opens one
//! connection per remote mount and, on it:
//!
//! * one long-lived *control* stream (first byte 0): a `Hello` with the share id and ticket
//!   secret, a `HelloReply`, then y-sync messages in both directions, each framed as
//!   `u32 LE length + payload`. The owner side treats this exactly like a WebSocket client of
//!   the share document; a read-only grant drops incoming document updates.
//! * any number of short *blob* streams (first byte 1): `frame(hash)` → `frame(bytes)` (empty
//!   frame = not found). Both sides request blobs this way whenever their copy of the share
//!   document references a hash they do not have, and verify the hash before storing.
//!
//! Neither server needs to be reachable from the internet; iroh dials through relays and
//! upgrades to a direct path when it can.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{presets, Connection, ReadExactError, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use yrs::block::ClientID;
use yrs::sync::DefaultProtocol;
use yrs::sync::Protocol;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{Any, Map, Out, ReadTxn, Transact};

use crate::bridge::ChangeKind;
use crate::storage::MountInfo;
use crate::ticket::Ticket;
use crate::vault::{DocRegistry, LiveDoc};
use crate::ws::handle_payload;

pub const ALPN: &[u8] = b"replica/share/1";
const STREAM_CONTROL: u8 = 0;
const STREAM_BLOB: u8 = 1;
/// Absolute backstop for authenticated y-sync frames.
const MAX_SYNC_FRAME: usize = 256 * 1024 * 1024;
/// Pre-auth frames are tiny: a hello / hello reply and a hex hash.
const MAX_HELLO_FRAME: usize = 4096;
const MAX_HASH_FRAME: usize = 256;
/// Streams a peer may have open at once on one connection (control + blob fetches).
const MAX_STREAMS_PER_CONN: usize = 16;
/// Incoming p2p connections served at once, over all peers. The node id is public (it is in
/// every ticket), so anyone can connect; beyond this they wait in the QUIC accept queue.
const MAX_INCOMING_CONNS: usize = 64;
/// A peer must finish the hello this fast or the stream is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub version: u8,
    pub share_id: String,
    pub secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HelloReply {
    Ok { access: String, name: String, only_file: Option<String> },
    /// The grant is invalid or revoked: the mounting side gives up for good.
    Denied(String),
    /// Transient: the two servers speak different protocol versions; keep retrying.
    VersionMismatch { expected: u8 },
}

fn ae(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

// ------------------------------------------------------------------ endpoint + addresses

/// Bind the p2p endpoint with this server's identity. `relays`: empty = n0 defaults,
/// `["none"]` = no relays, otherwise custom relay URLs.
pub async fn bind_endpoint(secret: SecretKey, relays: &[String], port: u16) -> anyhow::Result<Endpoint> {
    let mut b = Endpoint::builder(presets::N0).secret_key(secret).alpns(vec![ALPN.to_vec()]);
    if port != 0 {
        b = b
            .clear_ip_transports()
            .bind_addr(std::net::SocketAddr::from(([0, 0, 0, 0], port)))
            .map_err(ae)?
            .bind_addr(std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)))
            .map_err(ae)?;
    }
    let relays: Vec<&str> = relays.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if relays == ["none"] {
        b = b.relay_mode(RelayMode::Disabled);
    } else if !relays.is_empty() {
        let urls = relays
            .iter()
            .map(|u| RelayUrl::from_str(u).map_err(|e| anyhow::anyhow!("bad relay url {u}: {e}")))
            .collect::<anyhow::Result<Vec<RelayUrl>>>()?;
        b = b.relay_mode(RelayMode::Custom(RelayMap::from_iter(urls)));
    }
    b.bind().await.map_err(ae).context("binding p2p endpoint")
}

/// Addresses to embed in a ticket, as printed by iroh (`relay:https://…`, `ip:1.2.3.4:5678`).
pub fn addr_strings(ep: &Endpoint) -> Vec<String> {
    ep.addr().addrs.iter().map(|a| a.to_string()).collect()
}

pub fn ticket_addr(t: &Ticket) -> anyhow::Result<EndpointAddr> {
    let id = PublicKey::from_str(&t.node_id).map_err(|e| anyhow::anyhow!("bad node id in ticket: {e}"))?;
    Ok(EndpointAddr::from_parts(id, parse_addrs(&t.addrs)))
}

fn parse_addrs(addrs: &[String]) -> Vec<TransportAddr> {
    addrs
        .iter()
        .filter_map(|s| {
            if let Some(u) = s.strip_prefix("relay:") {
                RelayUrl::from_str(u).ok().map(TransportAddr::Relay)
            } else if let Some(ip) = s.strip_prefix("ip:") {
                ip.parse().ok().map(TransportAddr::Ip)
            } else {
                None
            }
        })
        .collect()
}

// ------------------------------------------------------------------ framing

async fn write_frame(s: &mut SendStream, data: &[u8]) -> anyhow::Result<()> {
    s.write_all(&(data.len() as u32).to_le_bytes()).await.map_err(ae)?;
    s.write_all(data).await.map_err(ae)?;
    Ok(())
}

/// Read one frame. `max` is checked before allocating, so an unauthenticated peer can only
/// make us allocate what its context allows (hello: 4 KB, hash: 256 B, blob: the blob limit).
async fn read_frame(r: &mut RecvStream, max: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(_)) => return Ok(None),
        Err(e) => return Err(ae(e)),
    }
    let n = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(n <= max, "frame too large ({n} bytes, limit {max})");
    // Grow with the data instead of trusting the declared length up front, so a peer that
    // announces 256 MB and sends nothing costs nothing.
    let mut buf: Vec<u8> = Vec::with_capacity(n.min(1 << 20));
    let mut chunk = vec![0u8; 1 << 16];
    while buf.len() < n {
        let want = (n - buf.len()).min(chunk.len());
        r.read_exact(&mut chunk[..want]).await.map_err(ae)?;
        buf.extend_from_slice(&chunk[..want]);
    }
    Ok(Some(buf))
}

/// Decrements `doc.conns` when dropped, whichever way the sync loop ends.
struct ConnDec(Arc<LiveDoc>);
impl Drop for ConnDec {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        *self.0.last_active.lock() = std::time::Instant::now();
    }
}
fn scopeguard_dec(doc: Arc<LiveDoc>) -> ConnDec {
    ConnDec(doc)
}

fn start_message(doc: &LiveDoc) -> anyhow::Result<Vec<u8>> {
    let aw = doc.awareness.lock();
    let mut enc = EncoderV1::new();
    DefaultProtocol.start(&aw, &mut enc).map_err(ae)?;
    Ok(enc.to_vec())
}

// ------------------------------------------------------------------ sync loop (both sides)

/// Who to re-check periodically so a revoked ticket or deleted share ends the session.
struct RevokeCheck {
    share_id: String,
    secret: String,
}

/// Pump y-sync between `doc` and the control stream until the stream closes. Blob hashes
/// that arrive in document updates but are missing locally are sent to `wants`.
async fn sync_loop(
    docs: &Arc<DocRegistry>,
    doc: &Arc<LiveDoc>,
    send: &mut SendStream,
    recv: &mut RecvStream,
    writable: bool,
    wants: &mpsc::UnboundedSender<String>,
    revoke: Option<RevokeCheck>,
) -> anyhow::Result<()> {
    let protocol = DefaultProtocol;
    let mut rx = doc.tx.subscribe();
    write_frame(send, &start_message(doc)?).await?;
    let mut owned: HashSet<ClientID> = HashSet::new();
    let mut step1 = crate::limits::RequestBudget::new(6, Duration::from_secs(60));
    doc.conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _conn_guard = scopeguard_dec(doc.clone());
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.tick().await;

    // Anything referenced but missing right now (e.g. interrupted earlier).
    for h in missing_blobs(doc) {
        let _ = wants.send(h);
    }

    loop {
        tokio::select! {
            frame = read_frame(recv, MAX_SYNC_FRAME) => {
                let Some(data) = frame? else { break };
                let replies = {
                    let _apply = docs.apply_lock.lock();
                    let r = {
                        let mut aw = doc.awareness.lock();
                        let mut limits = crate::ws::PayloadLimits {
                            doc_bytes: (doc.approx_bytes.load(std::sync::atomic::Ordering::Relaxed), docs.max_doc_bytes),
                            step1: &mut step1,
                        };
                        handle_payload(&protocol, &mut aw, &data, &mut owned, writable, &mut limits)
                    };
                    for h in new_blob_hashes(doc) {
                        let _ = wants.send(h);
                    }
                    docs.propagate(doc);
                    r
                };
                for r in replies.map_err(ae)? {
                    write_frame(send, &r).await?;
                }
            }
            b = rx.recv() => match b {
                Ok(bytes) => write_frame(send, &bytes).await?,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(doc = %doc.label, "peer lagged {n} messages, resyncing");
                    write_frame(send, &start_message(doc)?).await?;
                }
                Err(RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                if let Some(rc) = &revoke {
                    let ok = docs.storage().registry().check_grant(&rc.share_id, &rc.secret).ok().flatten().is_some();
                    if !ok {
                        tracing::info!(doc = %doc.label, "grant revoked or share gone; closing link");
                        break;
                    }
                }
            }
        }
    }
    if !owned.is_empty() {
        let mut aw = doc.awareness.lock();
        for id in owned {
            aw.remove_state(id);
        }
    }
    Ok(())
}

/// Blob hashes in changes recorded since the last propagate that the store lacks.
/// Must be called with `apply_lock` held, before `propagate` drains the changes.
fn new_blob_hashes(doc: &LiveDoc) -> Vec<String> {
    let changes = doc.changes.lock();
    changes
        .iter()
        .filter_map(|c| match &c.kind {
            ChangeKind::BlobSet(a) => blob_hash(a),
            _ => None,
        })
        .filter(|h| !doc.store.has_blob(h))
        .collect()
}

fn blob_hash(meta: &Any) -> Option<String> {
    let Any::Map(m) = meta else { return None };
    match m.get("hash") {
        Some(Any::String(h)) if crate::ids::valid_hash(h) => Some(h.to_ascii_lowercase()),
        Some(Any::String(h)) => {
            tracing::warn!("ignoring blob metadata with invalid hash {:?}", h.chars().take(16).collect::<String>());
            None
        }
        _ => None,
    }
}

/// First 12 characters for log lines; safe on any length.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn missing_blobs(doc: &LiveDoc) -> Vec<String> {
    let aw = doc.awareness.lock();
    let txn = aw.doc().transact();
    let Some(blobs) = txn.get_map("blobs") else { return vec![] };
    blobs
        .iter(&txn)
        .filter_map(|(_, v)| match v {
            Out::Any(a) => blob_hash(&a),
            _ => None,
        })
        .filter(|h| !doc.store.has_blob(h))
        .collect()
}

// ------------------------------------------------------------------ blobs over the link

/// Serve blob requests on streams the peer opens. `doc` is set once the control handshake
/// has succeeded; before that every request answers "not found".
async fn serve_streams(conn: Connection, doc: Arc<Mutex<Option<Arc<LiveDoc>>>>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS_PER_CONN));
    loop {
        let (send, recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("peer opened too many streams; refusing one");
            continue; // dropping the streams resets them
        };
        let doc = doc.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = serve_one(send, recv, doc).await {
                tracing::debug!("blob stream ended: {e}");
            }
        });
    }
}

async fn serve_one(send: SendStream, mut recv: RecvStream, doc: Arc<Mutex<Option<Arc<LiveDoc>>>>) -> anyhow::Result<()> {
    let mut kind = [0u8; 1];
    recv.read_exact(&mut kind).await.map_err(ae)?;
    anyhow::ensure!(kind[0] == STREAM_BLOB, "unexpected stream kind {}", kind[0]);
    serve_blob_request(send, recv, doc).await
}

fn session_doc(session: &Arc<Session>) -> Arc<Mutex<Option<Arc<LiveDoc>>>> {
    Arc::new(Mutex::new(session.doc.lock().clone()))
}

/// Answer one blob request (stream kind byte already consumed): `frame(hash)` → `frame(bytes)`,
/// empty frame when unknown or when no document is bound to this connection yet.
async fn serve_blob_request(mut send: SendStream, mut recv: RecvStream, doc: Arc<Mutex<Option<Arc<LiveDoc>>>>) -> anyhow::Result<()> {
    let hash = read_frame(&mut recv, MAX_HASH_FRAME).await?.context("no hash")?;
    let hash = String::from_utf8(hash).context("hash not utf8")?;
    let doc = doc.lock().clone();
    let bytes = match doc {
        Some(d) if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => d.store.get_blob(&hash)?.unwrap_or_default(),
        _ => Vec::new(),
    };
    write_frame(&mut send, &bytes).await?;
    send.finish().map_err(ae)?;
    let _ = send.stopped().await;
    Ok(())
}

/// Fetch every hash arriving on `rx` from the peer into `doc.store`, then hand it to the
/// bridges so the vault on this side gets the bytes too.
async fn fetch_blobs(conn: Connection, docs: Arc<DocRegistry>, doc: Arc<LiveDoc>, mut rx: mpsc::UnboundedReceiver<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(hash) = rx.recv().await {
        if doc.store.has_blob(&hash) || !seen.insert(hash.clone()) {
            continue;
        }
        match fetch_one(&conn, &hash, docs.max_blob_bytes).await {
            Ok(Some(bytes)) => {
                if let Err(e) = doc.store.put_blob(&hash, &bytes) {
                    tracing::warn!(doc = %doc.label, "storing fetched blob failed: {e}");
                    continue;
                }
                tracing::info!(doc = %doc.label, "fetched blob {} ({} bytes) from peer", short(&hash), bytes.len());
                docs.blob_arrived(&doc, &hash);
            }
            Ok(None) => {
                // Peer does not have it (yet); allow a retry when it is referenced again.
                tracing::debug!(doc = %doc.label, "peer has no blob {}", short(&hash));
                seen.remove(&hash);
            }
            Err(e) => {
                tracing::warn!(doc = %doc.label, "fetching blob {} failed: {e}", short(&hash));
                seen.remove(&hash);
            }
        }
    }
}

async fn fetch_one(conn: &Connection, hash: &str, max_blob: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(ae)?;
    send.write_all(&[STREAM_BLOB]).await.map_err(ae)?;
    write_frame(&mut send, hash.as_bytes()).await?;
    send.finish().map_err(ae)?;
    let bytes = read_frame(&mut recv, max_blob).await?.context("peer closed blob stream")?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let got = hex::encode(Sha256::digest(&bytes));
    anyhow::ensure!(got == hash, "blob hash mismatch (wanted {hash}, got {got})");
    Ok(Some(bytes))
}

// ------------------------------------------------------------------ owner side (accept)

#[derive(Clone)]
pub struct ShareProtocol {
    pub docs: Arc<DocRegistry>,
    pub conns: Arc<tokio::sync::Semaphore>,
    pub limits: Arc<crate::limits::Limits>,
    pub max_per_peer: usize,
}

impl ShareProtocol {
    pub fn new(docs: Arc<DocRegistry>, limits: Arc<crate::limits::Limits>, max_per_peer: usize) -> Self {
        Self { docs, conns: Arc::new(tokio::sync::Semaphore::new(MAX_INCOMING_CONNS)), limits, max_per_peer }
    }
}

/// Per-connection state on the owner side: the share doc once the hello succeeded, and
/// whether a control stream has been claimed (one per connection).
#[derive(Default)]
struct Session {
    doc: Mutex<Option<Arc<LiveDoc>>>,
    control_taken: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for ShareProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShareProtocol")
    }
}

impl ProtocolHandler for ShareProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id().fmt_short().to_string();
        let peer_full = conn.remote_id().to_string();
        if self.limits.p2p_fails.lock().blocked(&peer_full) {
            tracing::warn!(peer = %peer, "p2p connection refused: too many failed hellos");
            conn.close(4u32.into(), b"blocked");
            return Ok(());
        }
        let Ok(_conn_permit) = self.conns.clone().try_acquire_owned() else {
            tracing::warn!(peer = %peer, "p2p connection refused: too many connections");
            conn.close(3u32.into(), b"busy");
            return Ok(());
        };
        let Some(_peer_slot) = self.limits.conns.acquire(&format!("p:{peer_full}"), self.max_per_peer) else {
            tracing::warn!(peer = %peer, "p2p connection refused: too many connections from this peer");
            conn.close(3u32.into(), b"busy");
            return Ok(());
        };
        tracing::info!(peer = %peer, "p2p connection accepted");
        let session: Arc<Session> = Default::default();
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS_PER_CONN));
        loop {
            let (send, recv) = match conn.accept_bi().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::info!(peer = %peer, "p2p connection closed: {e}");
                    break;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                tracing::warn!(peer = %peer, "too many open streams; refusing one");
                continue;
            };
            let docs = self.docs.clone();
            let session = session.clone();
            let conn2 = conn.clone();
            let peer2 = peer.clone();
            let limits = self.limits.clone();
            let peer_full2 = peer_full.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = owner_stream(docs, conn2, session, send, recv, &peer2, &limits, &peer_full2).await {
                    tracing::debug!(peer = %peer2, "p2p stream ended: {e}");
                }
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn owner_stream(
    docs: Arc<DocRegistry>,
    conn: Connection,
    session: Arc<Session>,
    mut send: SendStream,
    mut recv: RecvStream,
    peer: &str,
    limits: &Arc<crate::limits::Limits>,
    peer_node: &str,
) -> anyhow::Result<()> {
    let mut kind = [0u8; 1];
    tokio::time::timeout(HELLO_TIMEOUT, recv.read_exact(&mut kind)).await.map_err(|_| anyhow::anyhow!("stream kind timeout"))?.map_err(ae)?;
    match kind[0] {
        STREAM_BLOB => serve_blob_request(send, recv, session_doc(&session)).await,
        STREAM_CONTROL => {
            if session.control_taken.swap(true, std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("second control stream on one connection");
            }
            let hello_frame = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv, MAX_HELLO_FRAME))
                .await
                .map_err(|_| anyhow::anyhow!("hello timeout"))??
                .context("no hello")?;
            let hello: Hello = postcard::from_bytes(&hello_frame).context("bad hello")?;
            if hello.version != PROTOCOL_VERSION {
                tracing::warn!(peer = %peer, "p2p hello with protocol v{}, we speak v{PROTOCOL_VERSION}", hello.version);
                let reply = postcard::to_stdvec(&HelloReply::VersionMismatch { expected: PROTOCOL_VERSION })?;
                write_frame(&mut send, &reply).await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
                conn.close(2u32.into(), b"version");
                return Ok(());
            }
            let reg = docs.storage().registry();
            let share = reg.share_by_id(&hello.share_id)?;
            let grant = match &share {
                Some(s) => reg.check_grant(&s.id, &hello.secret)?,
                None => None,
            };
            let (share, (label, access)) = match (share, grant) {
                (Some(s), Some(g)) => (s, g),
                _ => {
                    limits.p2p_fails.lock().record_failure(peer_node.to_string());
                    tracing::warn!(peer = %peer, share = %hello.share_id, "p2p hello denied");
                    let reply = postcard::to_stdvec(&HelloReply::Denied("ticket is invalid or has been revoked".into()))?;
                    write_frame(&mut send, &reply).await?;
                    let _ = send.finish();
                    // Let the reply drain before closing, otherwise the peer sees a plain
                    // "connection lost" and keeps retrying instead of marking the mount revoked.
                    let _ = tokio::time::timeout(Duration::from_secs(3), send.stopped()).await;
                    conn.close(1u32.into(), b"denied");
                    return Ok(());
                }
            };
            let doc = docs.share_doc_for_link(&share)?;
            *session.doc.lock() = Some(doc.clone());
            let reply = HelloReply::Ok { access: access.clone(), name: share.name.clone(), only_file: share.only_file.clone() };
            write_frame(&mut send, &postcard::to_stdvec(&reply)?).await?;
            tracing::info!(peer = %peer, share = %share.name, "p2p link up via {label} ({access})");

            let (wants_tx, wants_rx) = mpsc::unbounded_channel();
            let fetcher = tokio::spawn(fetch_blobs(conn.clone(), docs.clone(), doc.clone(), wants_rx));
            let res = sync_loop(
                &docs,
                &doc,
                &mut send,
                &mut recv,
                access == "rw",
                &wants_tx,
                Some(RevokeCheck { share_id: share.id.clone(), secret: hello.secret.clone() }),
            )
            .await;
            fetcher.abort();
            conn.close(0u32.into(), b"bye");
            tracing::info!(peer = %peer, share = %share.name, "p2p link down");
            res
        }
        k => anyhow::bail!("unknown stream kind {k}"),
    }
}

// ------------------------------------------------------------------ mounting side (connect)

/// Connect and complete the hello handshake, returning what the owner granted. Used by the
/// mount API before anything is stored, so a bad ticket fails fast.
pub async fn probe(ep: &Endpoint, ticket: &Ticket) -> anyhow::Result<(String, String, Option<String>)> {
    let addr = ticket_addr(ticket)?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN))
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to the owner server"))?
        .map_err(|e| anyhow::anyhow!("cannot reach the owner server: {e}"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(ae)?;
    let reply = handshake(&mut send, &mut recv, ticket).await;
    conn.close(0u32.into(), b"probe");
    match reply? {
        HelloReply::Ok { access, name, only_file } => Ok((access, name, only_file)),
        HelloReply::Denied(msg) => anyhow::bail!("owner server refused the ticket: {msg}"),
        HelloReply::VersionMismatch { expected } => {
            anyhow::bail!("owner server speaks sharing protocol v{expected}, this server v{PROTOCOL_VERSION}; upgrade one of them")
        }
    }
}

async fn handshake(send: &mut SendStream, recv: &mut RecvStream, ticket: &Ticket) -> anyhow::Result<HelloReply> {
    send.write_all(&[STREAM_CONTROL]).await.map_err(ae)?;
    let hello = Hello { version: PROTOCOL_VERSION, share_id: ticket.share_id.clone(), secret: ticket.secret.clone() };
    write_frame(send, &postcard::to_stdvec(&hello)?).await?;
    let reply = tokio::time::timeout(CONNECT_TIMEOUT, read_frame(recv, MAX_HELLO_FRAME))
        .await
        .map_err(|_| anyhow::anyhow!("owner server did not answer the hello"))??
        .context("owner server closed the stream")?;
    postcard::from_bytes(&reply).context("bad hello reply")
}

/// Keep one remote mount synced for as long as the task lives. Reconnects with backoff;
/// stops for good (and marks the mount revoked) when the owner denies the secret.
pub fn spawn_link(docs: Arc<DocRegistry>, mount: MountInfo, replica: Arc<LiveDoc>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(ep) = docs.endpoint.clone() else { return };
        let mut backoff = Duration::from_secs(2);
        loop {
            docs.set_link_status(&mount.id, "connecting");
            match run_link(&ep, &docs, &mount, &replica).await {
                Ok(()) => {
                    docs.set_link_status(&mount.id, "disconnected");
                    backoff = Duration::from_secs(2);
                }
                Err(LinkError::Denied(msg)) => {
                    tracing::warn!(mount = %mount.share_name, "owner server denied this mount: {msg}");
                    docs.set_link_status(&mount.id, &format!("revoked: {msg}"));
                    // Persist first, then detach. We are the link task, so the bridge removal
                    // must not abort us mid-cleanup (`detach_own_mount` only forgets the handle).
                    let _ = docs.storage().registry().revoke_mount(&mount.id);
                    docs.detach_own_mount(&mount.id);
                    return;
                }
                Err(LinkError::Other(e)) => {
                    tracing::warn!(mount = %mount.share_name, "p2p link failed: {e:#}; retrying in {backoff:?}");
                    docs.set_link_status(&mount.id, &format!("reconnecting ({e})"));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    })
}

enum LinkError {
    Denied(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for LinkError {
    fn from(e: anyhow::Error) -> Self {
        LinkError::Other(e)
    }
}

async fn run_link(ep: &Endpoint, docs: &Arc<DocRegistry>, mount: &MountInfo, replica: &Arc<LiveDoc>) -> Result<(), LinkError> {
    let ticket = Ticket {
        version: 1,
        share_id: mount.share_id.clone(),
        name: mount.share_name.clone(),
        node_id: mount.home_node.clone(),
        secret: mount.secret.clone(),
        access: mount.access.clone(),
        addrs: mount.home_addrs.clone(),
        url: String::new(),
    };
    let addr = ticket_addr(&ticket)?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN))
        .await
        .map_err(|_| anyhow::anyhow!("connect timed out"))?
        .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(ae)?;
    match handshake(&mut send, &mut recv, &ticket).await? {
        HelloReply::Denied(msg) => return Err(LinkError::Denied(msg)),
        HelloReply::VersionMismatch { expected } => {
            return Err(LinkError::Other(anyhow::anyhow!(
                "owner speaks sharing protocol v{expected}, we speak v{PROTOCOL_VERSION}; upgrade one side"
            )))
        }
        HelloReply::Ok { access, .. } => {
            tracing::info!(mount = %mount.share_name, "p2p link up to {} ({access})", conn.remote_id().fmt_short());
        }
    }
    docs.set_link_status(&mount.id, "connected");

    let session: Arc<Mutex<Option<Arc<LiveDoc>>>> = Arc::new(Mutex::new(Some(replica.clone())));
    let server = tokio::spawn(serve_streams(conn.clone(), session));
    let (wants_tx, wants_rx) = mpsc::unbounded_channel();
    let fetcher = tokio::spawn(fetch_blobs(conn.clone(), docs.clone(), replica.clone(), wants_rx));
    let res = sync_loop(docs, replica, &mut send, &mut recv, true, &wants_tx, None).await;
    server.abort();
    fetcher.abort();
    conn.close(0u32.into(), b"bye");
    tracing::info!(mount = %mount.share_name, "p2p link down");
    res.map_err(LinkError::Other)
}
