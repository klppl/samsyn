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

use crate::access::Access;
use crate::config::extract_token;
use crate::storage::ShareInfo;
use crate::vault::LiveDoc;
use crate::SharedState;

/// Guest sessions live this long; the client exchanges the secret again afterwards.
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// A short-lived stand-in for the ticket secret, so the secret itself never rides in a URL
/// after the first exchange. Revocation still applies: every use re-checks the grant.
pub struct GuestSession {
    share_id: String,
    secret: String,
    expires: Instant,
}

#[derive(Default)]
pub struct GuestSessions {
    map: HashMap<String, GuestSession>,
}

impl GuestSessions {
    fn create(&mut self, share_id: &str, secret: &str) -> String {
        if self.map.len() > 50_000 {
            self.map.retain(|_, s| s.expires > Instant::now());
        }
        if self.map.len() >= 50_000 {
            // Still full after pruning: drop the oldest half rather than grow without bound.
            let mut v: Vec<(String, Instant)> = self.map.iter().map(|(k, s)| (k.clone(), s.expires)).collect();
            v.sort_by_key(|(_, e)| *e);
            for (k, _) in v.into_iter().take(self.map.len() / 2) {
                self.map.remove(&k);
            }
        }
        let id = format!("gs_{}", hex::encode({
            let mut b = [0u8; 32];
            rand::Rng::fill_bytes(&mut rand::rng(), &mut b);
            b
        }));
        self.map.insert(id.clone(), GuestSession { share_id: share_id.into(), secret: secret.into(), expires: Instant::now() + SESSION_TTL });
        id
    }
    /// The secret behind a session id, if it is valid for this share.
    fn secret_for(&mut self, id: &str, share_id: &str) -> Option<String> {
        match self.map.get(id) {
            Some(s) if s.expires <= Instant::now() => {
                self.map.remove(id);
                None
            }
            // A session for another share is simply not valid here; it stays usable there.
            Some(s) if s.share_id == share_id => Some(s.secret.clone()),
            _ => None,
        }
    }
}

fn client_ip(state: &SharedState, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if !state.serve.trust_proxy {
        return peer.ip();
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next_back())
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
    pub rw_paths: Vec<String>,
    pub secret: String,
}

impl Guest {
    fn effective(&self) -> Access {
        Access::new(&self.access, &self.rw_paths)
    }
}

/// Validate `(share_id, secret)`; the secret comes as `?secret=` or `Authorization: Bearer`.
fn authenticate(state: &SharedState, share_id: &str, headers: &HeaderMap, query: &HashMap<String, String>, peer: SocketAddr) -> Result<Guest, Response> {
    let ip = client_ip(state, headers, peer);
    let presented = extract_token(headers, query.get("secret").or_else(|| query.get("session")).map(String::as_str)).unwrap_or_default();
    // A session id stands in for the secret it was created with.
    // Session ids are prefixed so they can never be mistaken for (or collide with) a secret.
    let is_session = presented.starts_with("gs_");
    let secret = if is_session {
        state.guest_sessions.lock().secret_for(&presented, share_id).unwrap_or_default()
    } else {
        presented
    };
    let reg = state.storage.registry();
    let found = match reg.share_by_id(share_id) {
        Ok(Some(s)) if secret.len() == 64 => match reg.check_grant(&s.id, &secret) {
            Ok(Some(g)) => Some(Guest { share: s, label: g.label, access: g.access, rw_paths: g.rw_paths, secret: secret.clone() }),
            _ => None,
        },
        _ => None,
    };
    match found {
        Some(g) => {
            let _ = reg.touch_grant(&g.share.id, &g.secret);
            Ok(g)
        }
        None if is_session && secret.is_empty() => {
            // Unknown or expired session: sessions are 256-bit ids we issued ourselves and they
            // all die with a restart, so a client with pending blob fetches produces a burst of
            // these. Not a guess at a secret; it must not trip the per-address budget (behind a
            // proxy that would lock out every guest). The client re-exchanges its ticket.
            tracing::debug!(share = %share_id, %ip, "guest session unknown or expired");
            Err(denied())
        }
        None => {
            // A valid secret is never refused for the address's past failures: the budget slows
            // guessing, and behind a proxy one address is many clients.
            let mut fails = state.limits.guest_fails.lock();
            if fails.blocked(&ip) {
                return Err((StatusCode::TOO_MANY_REQUESTS, "too many failed attempts; try again later").into_response());
            }
            fails.record_failure(ip);
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

// ------------------------------------------------------------------ POST /guest/session/{id}

/// Exchange the ticket secret for a session id. Clients then connect with `?session=` (or a
/// Bearer of the session), keeping the secret out of URLs and server logs for good.
pub async fn session(
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
    let id = state.guest_sessions.lock().create(&g.share.id, &g.secret);
    Json(serde_json::json!({
        "session": id,
        "expires_in": SESSION_TTL.as_secs(),
        "share_id": g.share.id,
        "name": g.share.name,
        "access": g.access,
        "rw_paths": g.rw_paths,
        "only_file": g.share.only_file,
    }))
    .into_response()
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
        "rw_paths": g.rw_paths,
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
    let access = g.effective();
    tracing::info!(share = %g.share.name, label = %g.label, access = %crate::access::describe(&g.access, &g.rw_paths), "guest connected");
    let writable = access.any_write();
    let Some(slot) = state.limits.conns.acquire(&format!("g:{}", crate::ids::token_hash(&g.secret)), state.serve.max_conns_per_ticket) else {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections for this ticket").into_response();
    };
    let check = Some(crate::ws::SessionCheck::Guest(g.share.id.clone(), g.secret.clone()));
    let cap = if writable { crate::ws::MAX_WS_MESSAGE } else { crate::ws::MAX_WS_MESSAGE_READONLY };
    ws.max_message_size(cap)
        .max_frame_size(cap)
        .on_upgrade(move |socket| crate::ws::handle_socket(socket, state, doc, crate::ws::Write::from_access(&access), check, slot))
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
    // Blobs are content-addressed, so the key is unknown here: any writable path is enough.
    // The metadata that references the hash goes through the per-key check on the socket.
    if !g.effective().any_write() {
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
    let quota = state.serve.max_share_mb * 1024 * 1024;
    // Check and write under one lock, otherwise parallel uploads all pass the check.
    let _quota = doc.quota_lock.lock();
    if quota > 0 && !doc.store.has_blob(&actual) {
        let used = doc.store.blob_bytes().unwrap_or(0);
        if used + body.len() as u64 > quota {
            tracing::warn!(share = %g.share.name, "guest upload refused: share quota of {} MB reached", state.serve.max_share_mb);
            return (StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({ "error": format!("this share's upload quota ({} MB) is full; ask the owner", state.serve.max_share_mb) }))).into_response();
        }
    }
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
