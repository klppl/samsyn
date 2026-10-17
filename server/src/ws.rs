//! WebSocket endpoint speaking the y-sync protocol, scoped to the authenticated user's vault.

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use yrs::block::ClientID;
use yrs::sync::{DefaultProtocol, Message, MessageReader, Protocol, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encoder, EncoderV1};

use crate::config::extract_token;
use crate::ids::valid_name;
use crate::storage::User;
use crate::vault::encode_message;
use crate::SharedState;

/// Largest y-sync message accepted from a client. A full vault state is one message, so this
/// bounds vault size over WebSocket; it also bounds what one client can make us allocate.
pub const MAX_WS_MESSAGE: usize = 256 * 1024 * 1024;

/// Client ids one socket may introduce into a document's awareness.
pub const MAX_AWARENESS_CLIENTS: usize = 64;
/// Read-only guests send awareness only; their inbound messages are capped much lower.
pub const MAX_WS_MESSAGE_READONLY: usize = 4 * 1024 * 1024;

/// Shared auth for ws + blob routes.
pub fn authenticate(state: &SharedState, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<User, Response> {
    let token = extract_token(headers, query.get("token").map(String::as_str));
    match token.and_then(|t| state.storage.registry().authenticate(&t).ok().flatten()) {
        Some(u) => Ok(u),
        None => Err((StatusCode::UNAUTHORIZED, "bad token").into_response()),
    }
}

pub async fn whoami(
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    match authenticate(&state, &headers, &query) {
        Ok(u) => {
            let vaults: Vec<String> = state
                .storage
                .registry()
                .list_vaults(Some(&u.id))
                .map(|v| v.into_iter().map(|v| v.name).collect())
                .unwrap_or_default();
            let used = state.limits.usage.get(&u.id, || state.storage.user_bytes(&u.id)).unwrap_or(0);
            let quota = state.serve.max_user_mb * 1024 * 1024;
            Json(serde_json::json!({ "user": u.name, "vaults": vaults, "usage_bytes": used, "quota_bytes": quota })).into_response()
        }
        Err(r) => r,
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(vault_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    if !valid_name(&vault_name) {
        return (StatusCode::BAD_REQUEST, "invalid vault name").into_response();
    }
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let token = extract_token(&headers, query.get("token").map(String::as_str)).unwrap_or_default();
    let Some(slot) = state.limits.conns.acquire(&format!("u:{}", crate::ids::token_hash(&token)), state.serve.max_conns_per_token) else {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections for this token").into_response();
    };
    let vault = match state.docs.get_or_load(&user, &vault_name) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("vault limit") => {
            return (StatusCode::CONFLICT, e.to_string()).into_response();
        }
        Err(e) => {
            tracing::error!(vault = %vault_name, user = %user.name, "failed to load vault: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load vault").into_response();
        }
    };
    let _ = user;
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| handle_socket(socket, state, vault, true, Some(SessionCheck::User(token)), slot))
}

/// What keeps a socket alive; re-checked every 30 s so a revoked token or ticket ends the
/// session instead of living on until the next reconnect.
pub enum SessionCheck {
    /// A user token (`rp_…`).
    User(String),
    /// A ticket: (share id, secret).
    Guest(String, String),
}

/// Pump y-sync between one WebSocket and a live document. `writable == false` drops the
/// client's document updates (read-only guests).
pub async fn handle_socket(
    socket: WebSocket,
    state: SharedState,
    vault: std::sync::Arc<crate::vault::LiveDoc>,
    writable: bool,
    check: Option<SessionCheck>,
    _slot: crate::limits::ConnSlot,
) {
    let (mut sink, mut stream) = socket.split();
    vault.conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *vault.last_active.lock() = std::time::Instant::now();
    let mut step1 = crate::limits::RequestBudget::new(6, std::time::Duration::from_secs(60));
    let mut rx = vault.tx.subscribe();
    let protocol = DefaultProtocol;
    let label = vault.label.clone();
    let mut revoke_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    revoke_tick.tick().await;

    let hello = {
        let aw = vault.awareness.lock();
        let mut enc = EncoderV1::new();
        match protocol.start(&aw, &mut enc) {
            Ok(()) => Bytes::from(enc.to_vec()),
            Err(e) => {
                tracing::warn!(vault = %label, "protocol start failed: {e}");
                return;
            }
        }
    };
    if sink.send(WsMessage::Binary(hello)).await.is_err() {
        return;
    }

    let mut owned_clients: HashSet<ClientID> = HashSet::new();
    tracing::debug!(vault = %label, "client connected");

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let replies = {
                            // Serialize all CRDT applies server-wide; see DocRegistry.
                            let _apply = state.docs.apply_lock.lock();
                            let r = {
                                let mut aw = vault.awareness.lock();
                                let mut limits = PayloadLimits {
                                    doc_bytes: (vault.approx_bytes.load(std::sync::atomic::Ordering::Relaxed), state.docs.max_doc_bytes),
                                    step1: &mut step1,
                                };
                                handle_payload(&protocol, &mut aw, &data, &mut owned_clients, writable, &mut limits)
                            };
                            state.docs.propagate(&vault);
                            r
                        };
                        match replies {
                            Ok(replies) => {
                                for r in replies {
                                    if sink.send(WsMessage::Binary(r)).await.is_err() { break; }
                                }
                            }
                            Err(e) => tracing::warn!(vault = %label, "bad message from client: {e}"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        if sink.send(WsMessage::Pong(p)).await.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(vault = %label, "ws error: {e}");
                        break;
                    }
                }
            }
            _ = revoke_tick.tick() => {
                let reg = state.storage.registry();
                let ok = match &check {
                    Some(SessionCheck::User(token)) => reg.authenticate(token).ok().flatten().is_some(),
                    Some(SessionCheck::Guest(share_id, secret)) => reg.check_grant(share_id, secret).ok().flatten().is_some(),
                    None => true,
                };
                if !ok {
                    tracing::info!(doc = %label, "credential revoked; closing socket");
                    break;
                }
            }
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(bytes) => {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() { break; }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(vault = %label, "client lagged {n} messages, resyncing");
                        let resync = {
                            let aw = vault.awareness.lock();
                            let mut enc = EncoderV1::new();
                            let _ = protocol.start(&aw, &mut enc);
                            Bytes::from(enc.to_vec())
                        };
                        if sink.send(WsMessage::Binary(resync)).await.is_err() { break; }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    if !owned_clients.is_empty() {
        let mut aw = vault.awareness.lock();
        for id in owned_clients {
            aw.remove_state(id);
        }
    }
    vault.conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    *vault.last_active.lock() = std::time::Instant::now();
    state.docs.maybe_compact(&vault);
    tracing::debug!(vault = %label, "client disconnected");
}

/// Feed one y-sync payload to the document. With `writable == false` (read-only peer) the
/// peer's document updates are dropped; it can still pull our state and send awareness.
/// Limits applied while feeding a payload: document size and full-state request budget.
pub(crate) struct PayloadLimits<'a> {
    /// (current approximate size, max) in bytes; `max == 0` means unlimited.
    pub doc_bytes: (u64, u64),
    pub step1: &'a mut crate::limits::RequestBudget,
}

pub(crate) fn handle_payload(
    protocol: &DefaultProtocol,
    aw: &mut yrs::sync::Awareness,
    data: &[u8],
    owned: &mut HashSet<ClientID>,
    writable: bool,
    limits: &mut PayloadLimits<'_>,
) -> Result<Vec<Bytes>, yrs::sync::Error> {
    let mut decoder = DecoderV1::from(data);
    let reader = MessageReader::new(&mut decoder);
    let mut out = Vec::new();
    for msg in reader {
        let msg = msg?;
        if !writable && matches!(msg, Message::Sync(SyncMessage::Update(_)) | Message::Sync(SyncMessage::SyncStep2(_))) {
            continue;
        }
        if let Message::Sync(SyncMessage::Update(u)) | Message::Sync(SyncMessage::SyncStep2(u)) = &msg {
            let (cur, max) = limits.doc_bytes;
            if max > 0 && cur + u.len() as u64 > max {
                tracing::warn!("document would exceed {} MB; update of {} bytes refused", max / 1_048_576, u.len());
                continue;
            }
        }
        if let Message::Sync(SyncMessage::SyncStep1(_)) = &msg {
            // A full-state request makes us encode the whole document; a few per connection
            // are normal (connect, resync), a stream of them is not.
            if !limits.step1.allow() {
                tracing::warn!("too many full-state requests on one connection; ignored");
                continue;
            }
        }
        if let Message::Awareness(update) = &msg {
            // One connection speaks for a handful of clients at most; a message carrying
            // thousands of ids is an attempt to bloat everyone's awareness state.
            if owned.len() + update.clients.len() > MAX_AWARENESS_CLIENTS {
                tracing::warn!("awareness update with {} clients ignored", update.clients.len());
                continue;
            }
            owned.extend(update.clients.keys().copied());
        }
        if let Some(reply) = protocol.handle_message(aw, msg)? {
            out.push(encode_message(&reply));
        }
    }
    Ok(out)
}
