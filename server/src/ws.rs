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
            Json(serde_json::json!({ "user": u.name, "vaults": vaults })).into_response()
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
    let vault = match state.docs.get_or_load(&user, &vault_name) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(vault = %vault_name, user = %user.name, "failed to load vault: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load vault").into_response();
        }
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, user, vault))
}

async fn handle_socket(
    socket: WebSocket,
    state: SharedState,
    _user: User,
    vault: std::sync::Arc<crate::vault::LiveDoc>,
) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = vault.tx.subscribe();
    let protocol = DefaultProtocol;
    let label = vault.label.clone();

    let hello = {
        let aw = vault.awareness.lock().unwrap();
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
                            let _apply = state.docs.apply_lock.lock().unwrap();
                            let r = {
                                let mut aw = vault.awareness.lock().unwrap();
                                handle_payload(&protocol, &mut aw, &data, &mut owned_clients, true)
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
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(bytes) => {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() { break; }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(vault = %label, "client lagged {n} messages, resyncing");
                        let resync = {
                            let aw = vault.awareness.lock().unwrap();
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
        let mut aw = vault.awareness.lock().unwrap();
        for id in owned_clients {
            aw.remove_state(id);
        }
    }
    state.docs.maybe_compact(&vault);
    tracing::debug!(vault = %label, "client disconnected");
}

/// Feed one y-sync payload to the document. With `writable == false` (read-only peer) the
/// peer's document updates are dropped; it can still pull our state and send awareness.
pub(crate) fn handle_payload(
    protocol: &DefaultProtocol,
    aw: &mut yrs::sync::Awareness,
    data: &[u8],
    owned: &mut HashSet<ClientID>,
    writable: bool,
) -> Result<Vec<Bytes>, yrs::sync::Error> {
    let mut decoder = DecoderV1::from(data);
    let reader = MessageReader::new(&mut decoder);
    let mut out = Vec::new();
    for msg in reader {
        let msg = msg?;
        if !writable && matches!(msg, Message::Sync(SyncMessage::Update(_)) | Message::Sync(SyncMessage::SyncStep2(_))) {
            continue;
        }
        if let Message::Awareness(update) = &msg {
            owned.extend(update.clients.keys().copied());
        }
        if let Some(reply) = protocol.handle_message(aw, msg)? {
            out.push(encode_message(&reply));
        }
    }
    Ok(out)
}
