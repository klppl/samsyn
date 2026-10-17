//! Content-addressed binary file storage (attachments, images, PDFs), per vault.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

use crate::ids::valid_name;
use crate::ws::authenticate;
use crate::SharedState;

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn put_blob(
    Path((vault_name, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    body: Bytes,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !valid_name(&vault_name) || !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }
    let actual = hex::encode(Sha256::digest(&body));
    if actual != hash.to_ascii_lowercase() {
        return (StatusCode::BAD_REQUEST, "hash mismatch").into_response();
    }
    let (_, store) = match state.storage.open_vault_for_user(&user, &vault_name, state.serve.max_vaults_per_user) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("vault limit") => return (StatusCode::CONFLICT, e.to_string()).into_response(),
        Err(e) => {
            tracing::error!("open vault failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match store.put_blob(&actual, &body) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => {
            tracing::error!("put_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn get_blob(
    Path((vault_name, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if !valid_name(&vault_name) || !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }
    // Reading never creates a vault.
    let (_, store) = match state.storage.existing_vault_for_user(&user, &vault_name) {
        Ok(Some(v)) => v,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("open vault failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match store.get_blob(&hash.to_ascii_lowercase()) {
        Ok(Some(data)) => ([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("get_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
