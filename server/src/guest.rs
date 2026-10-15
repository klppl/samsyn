//! Guest mode: a plugin (or headless client) with no server of its own joins a share by
//! talking to the owner's server directly, authenticated by the ticket secret instead of a
//! user token. The share document is what it syncs; the client maps share-relative keys to a
//! local folder. See docs/GUEST-MODE.md.
//!
//! These routes sit on a public server and the secret is a bearer credential, so: unknown share
//! and bad secret answer identically, failed attempts are rate limited per IP, and the request
//! log never carries the query string (see the TraceLayer span in main.rs).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{ws::WebSocketUpgrade, ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sha2::{Digest, Sha256};

use crate::config::extract_token;
use crate::storage::ShareInfo;
use crate::vault::LiveDoc;
use crate::SharedState;

const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);

/// Failed guest authentications per client IP: (count, window start).
#[derive(Default)]
pub struct GuestLimiter {
    fails: HashMap<IpAddr, (u32, Instant)>,
}

impl GuestLimiter {
    fn blocked(&mut self, ip: IpAddr) -> bool {
        match self.fails.get(&ip) {
            Some((n, since)) if since.elapsed() < FAILURE_WINDOW => *n >= MAX_FAILURES,
            Some(_) => {
                self.fails.remove(&ip);
                false
            }
            None => false,
        }
    }
    fn record_failure(&mut self, ip: IpAddr) {
        if self.fails.len() > 10_000 {
            self.fails.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
        }
        let e = self.fails.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= FAILURE_WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
}

fn denied() -> Response {
    (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "ticket is invalid or has been revoked" }))).into_response()
}

pub struct Guest {
    pub share: ShareInfo,
    pub label: String,
    pub access: String,
    pub secret: String,
}

/// Validate `(share_id, secret)`; the secret comes as `?secret=` or `Authorization: Bearer`.
fn authenticate(state: &SharedState, share_id: &str, headers: &HeaderMap, query: &HashMap<String, String>, peer: SocketAddr) -> Result<Guest, Response> {
    let ip = client_ip(headers, peer);
    if state.guest_limiter.lock().unwrap().blocked(ip) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many failed attempts; try again later").into_response());
    }
    let secret = extract_token(headers, query.get("secret").map(String::as_str)).unwrap_or_default();
    let reg = state.storage.registry();
    let found = match reg.share_by_id(share_id) {
        Ok(Some(s)) if secret.len() == 64 => match reg.check_grant(&s.id, &secret) {
            Ok(Some((label, access))) => Some(Guest { share: s, label, access, secret: secret.clone() }),
            _ => None,
        },
        _ => None,
    };
    match found {
        Some(g) => {
            let _ = reg.touch_grant(&g.share.id, &g.secret);
            Ok(g)
        }
        None => {
            state.guest_limiter.lock().unwrap().record_failure(ip);
            tracing::warn!(share = %share_id, %ip, "guest auth failed");
            Err(denied())
        }
    }
}

fn share_doc(state: &SharedState, share: &ShareInfo) -> Result<Arc<LiveDoc>, Response> {
    state.docs.share_doc_for_link(share).map_err(|e| {
        tracing::error!(share = %share.name, "loading share for guest failed: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "cannot load share").into_response()
    })
}

// ------------------------------------------------------------------ GET /guest/shares/{id}

pub async fn info(
    Path(share_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<SharedState>,
) -> Response {
    let g = match authenticate(&state, &share_id, &headers, &query, peer) {
        Ok(g) => g,
        Err(r) => return r,
    };
    Json(serde_json::json!({
        "share_id": g.share.id,
        "name": g.share.name,
        "access": g.access,
        "label": g.label,
        "only_file": g.share.only_file,
        "kind": if g.share.only_file.is_some() { "note" } else { "folder" },
        "node_id": state.docs.node_id,
    }))
    .into_response()
}

// ------------------------------------------------------------------ GET /guest/ws/{id}

pub async fn ws(
    ws: WebSocketUpgrade,
    Path(share_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<SharedState>,
) -> Response {
    let g = match authenticate(&state, &share_id, &headers, &query, peer) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let doc = match share_doc(&state, &g.share) {
        Ok(d) => d,
        Err(r) => return r,
    };
    tracing::info!(share = %g.share.name, label = %g.label, access = %g.access, "guest connected");
    let writable = g.access == "rw";
    let revoke = Some((g.share.id.clone(), g.secret.clone()));
    ws.on_upgrade(move |socket| crate::ws::handle_socket(socket, state, doc, writable, revoke))
}

// ------------------------------------------------------------------ blobs

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn get_blob(
    Path((share_id, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<SharedState>,
) -> Response {
    let g = match authenticate(&state, &share_id, &headers, &query, peer) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid hash").into_response();
    }
    let doc = match share_doc(&state, &g.share) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match doc.store.get_blob(&hash.to_ascii_lowercase()) {
        Ok(Some(data)) => ([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("guest get_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn put_blob(
    Path((share_id, hash)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<SharedState>,
    body: Bytes,
) -> Response {
    let g = match authenticate(&state, &share_id, &headers, &query, peer) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if g.access != "rw" {
        return (StatusCode::FORBIDDEN, "read-only ticket").into_response();
    }
    if !valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid hash").into_response();
    }
    let actual = hex::encode(Sha256::digest(&body));
    if actual != hash.to_ascii_lowercase() {
        return (StatusCode::BAD_REQUEST, "hash mismatch").into_response();
    }
    let doc = match share_doc(&state, &g.share) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match doc.store.put_blob(&actual, &body) {
        Ok(()) => {
            // The owner's vault will reference this hash once the metadata arrives; copy the
            // bytes over now so the owner's clients never see a 404.
            state.docs.blob_arrived(&doc, &actual);
            StatusCode::CREATED.into_response()
        }
        Err(e) => {
            tracing::error!("guest put_blob failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
