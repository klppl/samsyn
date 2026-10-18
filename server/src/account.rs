//! Per-device tokens for the authenticated user: list, issue, revoke. Lets the plugin set up
//! another device with its own token (shown as a QR code) that can be revoked on its own.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::config::extract_token;
use crate::ws::authenticate;
use crate::SharedState;

/// `?force=true` on a revoke: the caller insists on cutting off the token it is using.
fn forced(query: &HashMap<String, String>) -> bool {
    matches!(query.get("force").map(String::as_str), Some("1" | "true" | "yes"))
}

pub async fn list_tokens(Query(query): Query<HashMap<String, String>>, headers: HeaderMap, State(state): State<SharedState>) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let presented = extract_token(&headers, query.get("token").map(String::as_str));
    match state.storage.registry().list_tokens(&user.id, presented.as_deref()) {
        Ok(ts) => Json(serde_json::json!({
            "user": user.name,
            "tokens": ts
                .iter()
                .map(|t| serde_json::json!({ "label": t.label, "created_at": t.created_at, "last_used_at": t.last_used_at, "current": t.current }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct NewToken {
    pub label: Option<String>,
}

/// Issue a fresh token for another device. The plaintext is returned once.
pub async fn create_token(
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    body: Option<Json<NewToken>>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let label = body.and_then(|b| b.0.label).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).unwrap_or_else(|| "device".into());
    // Tokens are revoked by label, so one label must mean one token.
    match state.storage.registry().label_taken(&user.id, &label) {
        Ok(true) => return (StatusCode::CONFLICT, format!("a token labelled '{label}' already exists; pick another name")).into_response(),
        Ok(false) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
    match state.storage.registry().create_token(&user.id, &label) {
        Ok(token) => {
            tracing::info!(user = %user.name, "issued token '{label}'");
            Json(serde_json::json!({ "token": token, "label": label, "user": user.name })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn revoke_token(
    Path(label): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    // Revoking the token this very request carries would cut the device off mid-session; the
    // plugin hides the button for it, and anyone else must say `?force=true`.
    if !forced(&query) {
        let presented = extract_token(&headers, query.get("token").map(String::as_str)).unwrap_or_default();
        match state.storage.registry().is_token(&user.id, &label, &presented) {
            Ok(true) => {
                return (
                    StatusCode::CONFLICT,
                    format!("'{label}' is the token this device is using; set up another device token and switch to it first, or add ?force=true"),
                )
                    .into_response()
            }
            Ok(false) => {}
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
    match state.storage.registry().revoke_tokens(&user.id, Some(&label)) {
        Ok(0) => (StatusCode::NOT_FOUND, "no such token").into_response(),
        Ok(n) => {
            tracing::info!(user = %user.name, "revoked {n} token(s) labelled '{label}'");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
