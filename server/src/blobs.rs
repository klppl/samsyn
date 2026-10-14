//! Content-addressed binary file storage (attachments, images, PDFs).

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

use crate::config::extract_token;
use crate::vault::valid_vault_id;
use crate::SharedState;

fn authorize(state: &SharedState, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<(), Response> {
    let token = extract_token(headers, query.get("token").map(String::as_str));
    if state.config.token_matches(token.as_deref()) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "bad token").into_response())
    }
}

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn put_blob(
    Path((vault_id, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorize(&state, &headers, &query) {
        return r;
    }
    if !valid_vault_id(&vault_id) || !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }
    if body.len() > state.config.max_blob_mb * 1024 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "blob too large").into_response();
    }
    let actual = hex::encode(Sha256::digest(&body));
    if actual != hash.to_ascii_lowercase() {
        return (StatusCode::BAD_REQUEST, "hash mismatch").into_response();
    }
    match state.store.put_blob(&vault_id, &actual, &body) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => {
            tracing::error!("put_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn get_blob(
    Path((vault_id, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    if let Err(r) = authorize(&state, &headers, &query) {
        return r;
    }
    if !valid_vault_id(&vault_id) || !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }
    match state.store.get_blob(&vault_id, &hash.to_ascii_lowercase()) {
        Ok(Some(data)) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("get_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
