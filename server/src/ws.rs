//! WebSocket endpoint speaking the y-sync protocol.

use std::collections::HashMap;
use std::collections::HashSet;

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use yrs::block::ClientID;
use yrs::sync::{DefaultProtocol, Message, MessageReader, Protocol};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encoder, EncoderV1};

use crate::config::extract_token;
use crate::vault::{encode_message, valid_vault_id};
use crate::SharedState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(vault_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    if !valid_vault_id(&vault_id) {
        return (StatusCode::BAD_REQUEST, "invalid vault id").into_response();
    }
    let token = extract_token(&headers, query.get("token").map(String::as_str));
    if !state.config.token_matches(token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let vault = match state.vaults.get_or_load(&vault_id) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(vault = %vault_id, "failed to load vault: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load vault").into_response();
        }
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, vault_id, vault))
}

async fn handle_socket(
    socket: WebSocket,
    state: SharedState,
    vault_id: String,
    vault: std::sync::Arc<crate::vault::Vault>,
) {
    let (mut sink, mut stream) = socket.split();
    // Subscribe before the initial sync so nothing is missed in between.
    let mut rx = vault.tx.subscribe();
    let protocol = DefaultProtocol;

    // Initial handshake: our state vector + current awareness.
    let hello = {
        let aw = vault.awareness.lock().unwrap();
        let mut enc = EncoderV1::new();
        match protocol.start(&aw, &mut enc) {
            Ok(()) => Bytes::from(enc.to_vec()),
            Err(e) => {
                tracing::warn!(vault = %vault_id, "protocol start failed: {e}");
                return;
            }
        }
    };
    if sink.send(WsMessage::Binary(hello)).await.is_err() {
        return;
    }

    // Awareness client ids this connection has announced, cleaned up on disconnect.
    let mut owned_clients: HashSet<ClientID> = HashSet::new();
    tracing::debug!(vault = %vault_id, "client connected");

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let replies = {
                            let mut aw = vault.awareness.lock().unwrap();
                            handle_payload(&protocol, &mut aw, &data, &mut owned_clients)
                        };
                        match replies {
                            Ok(replies) => {
                                for r in replies {
                                    if sink.send(WsMessage::Binary(r)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(vault = %vault_id, "bad message from client: {e}");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        if sink.send(WsMessage::Pong(p)).await.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(vault = %vault_id, "ws error: {e}");
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
                        // Too slow; resend a full sync step so the client catches up.
                        tracing::warn!(vault = %vault_id, "client lagged {n} messages, resyncing");
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

    // Remove this connection's awareness states so others see it leave.
    if !owned_clients.is_empty() {
        let mut aw = vault.awareness.lock().unwrap();
        for id in owned_clients {
            aw.remove_state(id);
        }
    }
    state.vaults.maybe_compact(&vault);
    tracing::debug!(vault = %vault_id, "client disconnected");
}

/// Decode every message in a payload, track awareness client ids, run the protocol.
fn handle_payload(
    protocol: &DefaultProtocol,
    aw: &mut yrs::sync::Awareness,
    data: &[u8],
    owned: &mut HashSet<ClientID>,
) -> Result<Vec<Bytes>, yrs::sync::Error> {
    let mut decoder = DecoderV1::from(data);
    let reader = MessageReader::new(&mut decoder);
    let mut out = Vec::new();
    for msg in reader {
        let msg = msg?;
        if let Message::Awareness(update) = &msg {
            owned.extend(update.clients.keys().copied());
        }
        if let Some(reply) = protocol.handle_message(aw, msg)? {
            out.push(encode_message(&reply));
        }
    }
    Ok(out)
}
